//! p6-producer: the Project 6 production producer.
//!
//! Derives restic patch-mode changed/deleted path lists from a
//! `btrfs send --no-data` stream with a streaming TLV parser (no
//! `btrfs receive --dump` text, no metadata walks of the tree), and
//! drives the retained-snapshot lifecycle the Phase C drills
//! validated.
//!
//! Usage:
//!   p6-producer [--root R --repo REPO --restic BIN --owner NAME
//!                [--targets a,b] [--state-dir D] [--features F]]
//!       The full producer cycle (producer.py's run()); also spelled
//!       `run` with the same flags.
//!   p6-producer prepare  <same flags>
//!       Wrapper hook 1: snapshot + diff + per-target lists + a JSON
//!       manifest. Exit 0 patch-ready, 2 init needed, 3 fallback
//!       needed, 4 root is not on a btrfs filesystem (manifest mode
//!       "unsupported", no state touched), 1 hard failure.
//!   p6-producer finalize <same flags> --result success|fallback|init|abort
//!       Wrapper hook 2: rotate / re-base / establish the retained
//!       snapshot after the wrapper's backups have run.
//!   p6-producer parse --stream F|-
//!                     --retained DIR --fresh DIR
//!                     --changed-out FILE --deleted-out FILE
//!       Parse one send stream (retained -> fresh) and write the
//!       relative changed/deleted lists, one path per line. Prints
//!       a JSON summary line. Used by the validation gates and for
//!       debugging; the lifecycle drives the same code in-process.

mod crc32c;
mod ino;
mod treesearch;
mod parse;
mod producer;
mod resolve;
mod stream;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use parse::parse_stream;
use producer::{escape_json_bytes, Producer};
use stream::StreamReader;

fn flag_map(args: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(name) = args[i].strip_prefix("--") {
            if i + 1 < args.len() {
                map.insert(name.to_string(), args[i + 1].clone());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    map
}

fn producer_args(flags: &HashMap<String, String>) -> producer::Args {
    producer::Args {
        root: flags.get("root").cloned().unwrap_or_default(),
        repo: flags.get("repo").cloned().unwrap_or_default(),
        restic: flags.get("restic").cloned().unwrap_or_default(),
        owner: flags.get("owner").cloned().unwrap_or_default(),
        targets: flags
            .get("targets")
            .cloned()
            .unwrap_or_else(|| ".".to_string()),
        state_dir: flags.get("state-dir").cloned(),
        features: flags
            .get("features")
            .cloned()
            .unwrap_or_else(|| "patch-mode".to_string()),
    }
}

fn cmd_parse(flags: &HashMap<String, String>) -> i32 {
    let stream_arg = match flags.get("stream") {
        Some(s) => s.clone(),
        None => {
            eprintln!("parse: --stream is required");
            return 2;
        }
    };
    let retained = PathBuf::from(flags.get("retained").cloned().unwrap_or_default());
    let fresh = PathBuf::from(flags.get("fresh").cloned().unwrap_or_default());
    let changed_out = flags.get("changed-out").cloned().unwrap_or_default();
    let deleted_out = flags.get("deleted-out").cloned().unwrap_or_default();
    let outcome = if stream_arg == "-" {
        parse_stream(io::stdin().lock(), &retained, &fresh)
    } else {
        match fs::File::open(&stream_arg) {
            Ok(f) => parse_stream(f, &retained, &fresh),
            Err(e) => {
                eprintln!("parse: open {stream_arg}: {e}");
                return 1;
            }
        }
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            eprintln!("parse: {e}");
            return 1;
        }
    };
    let mut changed: Vec<&Vec<u8>> = outcome.changed.iter().collect();
    let mut deleted: Vec<&Vec<u8>> = outcome.deleted.iter().collect();
    changed.sort();
    deleted.sort();
    for (path, lines) in [(&changed_out, &changed), (&deleted_out, &deleted)] {
        let mut buf = Vec::new();
        for line in lines {
            buf.extend_from_slice(line);
            buf.push(b'\n');
        }
        if let Err(e) = fs::write(path, &buf) {
            eprintln!("parse: write {path}: {e}");
            return 1;
        }
    }
    let events: Vec<String> = outcome
        .events
        .iter()
        .map(|e| format!("{{\"kind\": \"{}\"}}", e.kind))
        .collect();
    println!(
        "{{\"changed\": {}, \"deleted\": {}, \"atime_only\": {}, \"version\": {}, \"commands\": {}, \"events\": [{}]}}",
        outcome.changed.len(),
        outcome.deleted.len(),
        outcome.atime_only,
        outcome.version,
        outcome.commands,
        events.join(", ")
    );
    0
}

fn cmd_ops(flags: &HashMap<String, String>) -> i32 {
    // Debugging aid: print the classified operation list for a
    // stream without resolving it.
    let stream_arg = flags.get("stream").cloned().unwrap_or_default();
    let src: Box<dyn io::Read> = if stream_arg == "-" {
        Box::new(io::stdin().lock())
    } else {
        match fs::File::open(&stream_arg) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("ops: open {stream_arg}: {e}");
                return 1;
            }
        }
    };
    let mut reader = match StreamReader::new(src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ops: {e}");
            return 1;
        }
    };
    loop {
        match reader.next_op() {
            Ok(Some(op)) => {
                let aux = op
                    .aux
                    .as_deref()
                    .map(|a| format!(" aux={}", escape_json_bytes(a)))
                    .unwrap_or_default();
                println!(
                    "{} {:?} {}{}",
                    op.seq,
                    op.kind,
                    escape_json_bytes(&op.path),
                    aux
                );
            }
            Ok(None) => return 0,
            Err(e) => {
                eprintln!("ops: {e}");
                return 1;
            }
        }
    }
}

fn cmd_ino(flags: &HashMap<String, String>) -> i32 {
    // Debugging aid: BTRFS_IOC_INO_PATHS for one inode.
    let root = PathBuf::from(flags.get("root").cloned().unwrap_or_default());
    let ino: u64 = flags
        .get("ino")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let f = match fs::File::open(&root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ino: open {}: {e}", root.display());
            return 1;
        }
    };
    match ino::ino_paths(&f, ino) {
        Ok(ino::InoResolution::Complete(paths)) => {
            for p in &paths {
                println!("{}", escape_json_bytes(p));
            }
            0
        }
        Ok(ino::InoResolution::Clamped) => {
            eprintln!(
                "ino: {ino} has more paths than the kernel's one-page container holds"
            );
            1
        }
        Err(e) => {
            eprintln!("ino: {e}");
            1
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let (sub, rest): (&str, &[String]) = match argv.first().map(|s| s.as_str()) {
        Some("run") => ("run", &argv[1..]),
        Some("prepare") => ("prepare", &argv[1..]),
        Some("finalize") => ("finalize", &argv[1..]),
        Some("parse") => ("parse", &argv[1..]),
        Some("ops") => ("ops", &argv[1..]),
        Some("ino") => ("ino", &argv[1..]),
        _ => ("run", &argv[..]),
    };
    let flags = flag_map(rest);
    let code = match sub {
        "parse" => cmd_parse(&flags),
        "ops" => cmd_ops(&flags),
        "ino" => cmd_ino(&flags),
        "prepare" => Producer::new(&producer_args(&flags)).prepare(),
        "finalize" => {
            let result = flags.get("result").cloned().unwrap_or_default();
            Producer::new(&producer_args(&flags)).finalize(&result)
        }
        _ => Producer::new(&producer_args(&flags)).run(),
    };
    ExitCode::from(code as u8)
}
