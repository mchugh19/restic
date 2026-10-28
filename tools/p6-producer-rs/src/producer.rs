//! The producer lifecycle: a port of tools/p6-producer/producer.py,
//! whose state machine the Phase C drills validated (81/81: crash at
//! every step, fallback re-base, two-producer ownership). The
//! semantics here follow that prototype exactly; where the two
//! could disagree, the drill outcomes are the arbiter.
//!
//! One producer owns one root (a btrfs subvolume) and one restic
//! repository. Its state lives in a state directory it owns alone
//! (default: a sibling of the root named .p6prod-<owner>-<rootname>):
//!
//!     retained   read-only snapshot of the root as of the last
//!                successful backup (the diff base)
//!     fresh      read-only snapshot taken by the current run (the
//!                diff target and the read source for the backup)
//!     lists/     the per-target list files of the most recent run
//!     state.json phase marker ("idle" / "backed-up" / "prepared")
//!
//! Run sequence (the crash drills kill the producer at each step):
//!   1. sweep own stale snapshots; adopt a backed-up fresh left by
//!      a run stopped before rotation, so the diff base again
//!      equals the repository parent.
//!   2. fresh := read-only snapshot of the root, sync.
//!   3. diff: `btrfs send --no-data` retained -> fresh, parsed by
//!      the streaming TLV parser into changed/deleted lists.
//!   4. backup: per target, a patch-mode backup reading from the
//!      fresh snapshot; it must engage, a decline is a producer
//!      failure. Records the "backed-up" phase marker.
//!   5. rotate: delete the old retained, rename fresh -> retained.
//!
//! Any failure in steps 3-4 falls back to a plain full backup of
//! every target from the fresh snapshot; afterwards the retained
//! snapshot is re-based to exactly the state just backed up, never
//! past it, which also heals a corrupt retained. With no retained
//! snapshot the run initialises instead. Exit codes for `run`:
//! 0 patch or init success, 3 fallback success, 1 hard failure.
//!
//! Subcommands for the wrapper's two hooks:
//!   prepare   steps 1-3 plus per-target list files and a JSON
//!             manifest on stdout. Exit 0: patch lists ready;
//!             2: initialisation needed (plain backup from fresh,
//!             then finalize --result init); 3: producer failure
//!             (plain backup from fresh, then finalize --result
//!             fallback); 4: the root is not on a btrfs filesystem
//!             (manifest mode "unsupported"; checked by statfs
//!             before any state is touched, so no snapshot, phase
//!             write or retained/fresh change happens); 1: hard
//!             failure.
//!   finalize  --result success: mark backed-up, then rotate
//!             (a crash inside finalize therefore leaves a
//!             backed-up marker the next prepare adopts).
//!             --result fallback: re-base retained to the fresh
//!             snapshot, the state the plain backup just recorded.
//!             --result init: establish retained := fresh.
//!             --result abort: leave everything as it is.
//! A wrapper crash between prepare and finalize --result success
//! leaves phase "prepared", which is deliberately NOT adopted:
//! the next prepare re-snapshots and re-diffs against the older
//! retained, patch mode declines if the lists no longer reconcile
//! with the advanced parent, and the fallback re-base heals the
//! base. The diff base never advances past a backed-up state.
//!
//! Drill hooks (environment): P6_KILL_AT=<step>|rotate-mid kills
//! the producer with SIGKILL at that point; P6_CORRUPT_STREAM=1
//! spools the send stream to disk and flips 512 bytes at its
//! midpoint before parsing; P6_SEND_FAIL=1 makes the send use a
//! nonexistent parent.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::parse::{parse_stream, ParseError};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
    fn statfs(path: *const std::ffi::c_char, buf: *mut u8) -> i32;
}

/// statfs(2) filesystem magic of the filesystem holding `path`
/// (the f_type word of struct statfs), or None when statfs itself
/// fails. Only f_type, the struct's first word, is read, so a
/// zeroed scratch buffer stands in for the full struct.
fn fs_magic(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = [0u8; 128];
    if unsafe { statfs(c.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return None;
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(&buf[..8]);
    Some(u64::from_ne_bytes(word))
}

/// BTRFS_SUPER_MAGIC (statfs f_type for btrfs filesystems).
const BTRFS_SUPER_MAGIC: u64 = 0x9123683E;

pub struct Args {
    pub root: String,
    pub repo: String,
    pub restic: String,
    pub owner: String,
    pub targets: String,
    pub state_dir: Option<String>,
    pub features: String,
}

fn log(msg: &str) {
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = writeln!(h, "{msg}");
    let _ = h.flush();
}

fn maybe_kill(point: &str) {
    if env::var("P6_KILL_AT").as_deref() == Ok(point) {
        log(&format!("KILL HOOK at {point}"));
        unsafe {
            kill(getpid(), 9);
        }
        std::process::exit(137);
    }
}

struct CmdOut {
    rc: i32,
    stdout: String,
    stderr: String,
}

fn run_cmd(argv: &[&str], extra_env: &[(&str, &str)]) -> Result<CmdOut, String> {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    Ok(CmdOut {
        rc: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Extract a string value for `key` from the flat JSON object the
/// phase marker is written as. Tolerates a null value (returns
/// None) and a missing key.
fn json_get_str(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let at = text.find(&pat)? + pat.len();
    let rest = text[at..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        if let Some(c) = char::from_u32(n) {
                            out.push(c);
                        }
                    }
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

pub struct Producer {
    root: PathBuf,
    repo: String,
    restic: String,
    targets: Vec<String>,
    features: String,
    state: PathBuf,
    retained: PathBuf,
    fresh: PathBuf,
    lists_dir: PathBuf,
    snapshot_s: f64,
    diff_s: f64,
    backup_s: f64,
}

impl Producer {
    pub fn new(args: &Args) -> Producer {
        let root = PathBuf::from(&args.root);
        let root = if root.is_absolute() {
            root
        } else {
            env::current_dir().unwrap_or_default().join(root)
        };
        // Normalise away any trailing slash / "." components the
        // way os.path.abspath does for the prototype's purposes.
        let root = normalize(&root);
        let state = match &args.state_dir {
            Some(s) => normalize(&PathBuf::from(s)),
            None => {
                let parent = root
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                let base = root
                    .file_name()
                    .map(|b| b.to_string_lossy().into_owned())
                    .unwrap_or_default();
                parent.join(format!(".p6prod-{}-{}", args.owner, base))
            }
        };
        let retained = state.join("retained");
        let fresh = state.join("fresh");
        let lists_dir = state.join("lists");
        Producer {
            root,
            repo: args.repo.clone(),
            restic: args.restic.clone(),
            targets: args.targets.split(',').map(|s| s.to_string()).collect(),
            features: args.features.clone(),
            state,
            retained,
            fresh,
            lists_dir,
            snapshot_s: 0.0,
            diff_s: 0.0,
            backup_s: 0.0,
        }
    }

    // -- btrfs helpers ------------------------------------------------
    fn is_subvolume(&self, path: &Path) -> bool {
        path.is_dir()
            && run_cmd(&["btrfs", "subvolume", "show", &path.to_string_lossy()], &[])
                .map(|o| o.rc == 0)
                .unwrap_or(false)
    }

    fn delete_subvolume(&self, path: &Path) -> Result<(), String> {
        if self.is_subvolume(path) {
            let o = run_cmd(
                &["btrfs", "subvolume", "delete", &path.to_string_lossy()],
                &[],
            )?;
            if o.rc != 0 {
                return Err(format!(
                    "delete {}: {}",
                    path.display(),
                    o.stderr.trim()
                ));
            }
        } else if path.symlink_metadata().is_ok() {
            return Err(format!(
                "{} exists and is not a subvolume",
                path.display()
            ));
        }
        Ok(())
    }

    fn snapshot_ro(&self, src: &Path, dst: &Path) -> Result<(), String> {
        self.delete_subvolume(dst)?;
        let o = run_cmd(
            &[
                "btrfs",
                "subvolume",
                "snapshot",
                "-r",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ],
            &[],
        )?;
        if o.rc != 0 {
            return Err(format!(
                "snapshot {} -> {}: {}",
                src.display(),
                dst.display(),
                o.stderr.trim()
            ));
        }
        Ok(())
    }

    fn subvolume_uuid(&self, path: &Path) -> Option<String> {
        let o = run_cmd(&["btrfs", "subvolume", "show", &path.to_string_lossy()], &[])
            .ok()?;
        for line in o.stdout.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("UUID:") {
                return Some(v.trim().to_string());
            }
        }
        None
    }

    fn remove_any(&self, path: &Path) -> Result<(), String> {
        if self.is_subvolume(path) {
            self.delete_subvolume(path)
        } else if path.is_dir() {
            fs::remove_dir_all(path).map_err(|e| format!("rmtree {}: {e}", path.display()))
        } else if path.symlink_metadata().is_ok() {
            fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))
        } else {
            Ok(())
        }
    }

    // -- restic --------------------------------------------------------
    fn restic_backup(
        &self,
        target_path: &Path,
        patch_lists: Option<(&Path, &Path)>,
    ) -> Result<CmdOut, String> {
        let mut argv: Vec<String> = vec![
            self.restic.clone(),
            "-r".into(),
            self.repo.clone(),
            "backup".into(),
            target_path.to_string_lossy().into_owned(),
        ];
        let mut envs: Vec<(&str, &str)> = Vec::new();
        if let Some((changed, deleted)) = patch_lists {
            argv.push("--patch-changed-from".into());
            argv.push(changed.to_string_lossy().into_owned());
            argv.push("--patch-deleted-from".into());
            argv.push(deleted.to_string_lossy().into_owned());
            envs.push(("RESTIC_FEATURES", &self.features));
        }
        let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_cmd(&refs, &envs)
    }

    // -- phase marker ----------------------------------------------------
    fn phase_file(&self) -> PathBuf {
        self.state.join("state.json")
    }

    fn read_phase(&self) -> (Option<String>, Option<String>) {
        let text = match fs::read_to_string(self.phase_file()) {
            Ok(t) => t,
            Err(_) => return (None, None),
        };
        (
            json_get_str(&text, "phase"),
            json_get_str(&text, "uuid"),
        )
    }

    fn write_phase(&self, phase: &str, snap_path: Option<&Path>) {
        let uuid = snap_path.and_then(|p| self.subvolume_uuid(p));
        let text = match uuid {
            Some(u) => format!(
                "{{\"phase\": \"{}\", \"uuid\": \"{}\"}}",
                json_escape(phase),
                json_escape(&u)
            ),
            None => format!("{{\"phase\": \"{}\"}}", json_escape(phase)),
        };
        let _ = fs::write(self.phase_file(), text);
    }

    // -- steps ----------------------------------------------------------
    fn step1_sweep(&self) -> Result<(), String> {
        maybe_kill("1");
        log("STEP 1: sweep");
        fs::create_dir_all(&self.state).map_err(|e| e.to_string())?;
        fs::create_dir_all(&self.lists_dir).map_err(|e| e.to_string())?;
        let (phase, uuid) = self.read_phase();
        if phase.as_deref() == Some("backed-up") {
            let fresh_uuid = if self.fresh.exists() {
                self.subvolume_uuid(&self.fresh)
            } else {
                None
            };
            if fresh_uuid.is_some() && fresh_uuid == uuid {
                // The previous run's backups succeeded but it was
                // stopped before rotating: adopt its fresh snapshot
                // as the diff base so base == repository parent.
                self.delete_subvolume(&self.retained)?;
                fs::rename(&self.fresh, &self.retained)
                    .map_err(|e| format!("adopt fresh: {e}"))?;
                log("STEP 1: adopted backed-up fresh as retained (previous run stopped before rotation)");
            } else {
                log("STEP 1: backed-up marker without its fresh snapshot; clearing the marker, retained kept");
            }
            self.write_phase("idle", Some(&self.retained));
        }
        let mut names: Vec<_> = match fs::read_dir(&self.state) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => Vec::new(),
        };
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if ["retained", "fresh", "lists", "state.json", "stream.bin", "dump.txt"]
                .contains(&name_s.as_ref())
            {
                continue;
            }
            let path = entry.path();
            if self.is_subvolume(&path) {
                self.delete_subvolume(&path)?;
                log(&format!("STEP 1: removed own stale snapshot {name_s}"));
            }
        }
        Ok(())
    }

    fn step2_snapshot(&mut self) -> Result<(), String> {
        maybe_kill("2");
        log("STEP 2: fresh snapshot");
        let t0 = Instant::now();
        self.snapshot_ro(&self.root, &self.fresh)?;
        let _ = run_cmd(&["sync"], &[]);
        self.snapshot_s = t0.elapsed().as_secs_f64();
        Ok(())
    }

    fn step3_diff(&mut self) -> Result<(HashSet<Vec<u8>>, HashSet<Vec<u8>>), String> {
        maybe_kill("3");
        log("STEP 3: send diff");
        let t0 = Instant::now();
        let parent = if env::var_os("P6_SEND_FAIL").is_some() {
            self.state.join("nonexistent-parent")
        } else {
            self.retained.clone()
        };
        let outcome = if env::var_os("P6_CORRUPT_STREAM").is_some() {
            self.diff_via_spooled_stream(&parent)
        } else {
            self.diff_via_pipe(&parent)
        };
        let (changed, deleted) = outcome.map_err(|e| e.to_string())?;
        self.diff_s = t0.elapsed().as_secs_f64();
        log(&format!(
            "STEP 3: {} changed, {} deleted",
            changed.len(),
            deleted.len()
        ));
        Ok((changed, deleted))
    }

    fn diff_via_pipe(
        &self,
        parent: &Path,
    ) -> Result<(HashSet<Vec<u8>>, HashSet<Vec<u8>>), ParseError> {
        let mut child = Command::new("btrfs")
            .args([
                "send",
                "--no-data",
                "-p",
                &parent.to_string_lossy(),
                &self.fresh.to_string_lossy(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ParseError::Io(e))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr_handle = child.stderr.take().expect("piped stderr");
        let drain = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_handle.read_to_end(&mut buf);
            buf
        });
        // If parsing fails, the reader is dropped with the outcome,
        // closing the pipe; the child then dies on its next write
        // and wait() reaps it.
        let parsed = parse_stream(stdout, &self.retained, &self.fresh);
        let status = child.wait().map_err(ParseError::Io)?;
        let stderr = drain.join().unwrap_or_default();
        let outcome = parsed?;
        if !status.success() {
            return Err(ParseError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "btrfs send failed: {}",
                    String::from_utf8_lossy(&stderr).trim()
                ),
            )));
        }
        Ok((outcome.changed, outcome.deleted))
    }

    fn diff_via_spooled_stream(
        &self,
        parent: &Path,
    ) -> Result<(HashSet<Vec<u8>>, HashSet<Vec<u8>>), ParseError> {
        // Drill path (P6_CORRUPT_STREAM): spool like the prototype,
        // flip 512 bytes at the midpoint, then parse from the file.
        let stream_path = self.state.join("stream.bin");
        let file = fs::File::create(&stream_path)?;
        let out = Command::new("btrfs")
            .args([
                "send",
                "--no-data",
                "-p",
                &parent.to_string_lossy(),
                &self.fresh.to_string_lossy(),
            ])
            .stdout(Stdio::from(
                file.try_clone().map_err(ParseError::Io)?,
            ))
            .stderr(Stdio::piped())
            .output()
            .map_err(ParseError::Io)?;
        if !out.status.success() {
            return Err(ParseError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "btrfs send failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            )));
        }
        drop(file);
        let mut data = fs::read(&stream_path)?;
        let mid = data.len() / 2;
        let end = (mid + 512).min(data.len());
        for b in &mut data[mid..end] {
            *b ^= 0xFF;
        }
        fs::write(&stream_path, &data)?;
        let file = fs::File::open(&stream_path)?;
        let outcome = parse_stream(file, &self.retained, &self.fresh)?;
        Ok((outcome.changed, outcome.deleted))
    }

    fn target_path(&self, target: &str) -> PathBuf {
        if target == "." {
            self.fresh.clone()
        } else {
            self.fresh.join(target)
        }
    }

    fn filter_lists(
        &self,
        target: &str,
        changed: &HashSet<Vec<u8>>,
        deleted: &HashSet<Vec<u8>>,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let keep = |rel: &[u8]| -> bool {
            if target == "." {
                return true;
            }
            let t = target.as_bytes();
            rel == t
                || (rel.len() > t.len() && rel.starts_with(t) && rel[t.len()] == b'/')
        };
        let fresh_b = self.fresh.as_os_str().as_bytes();
        let absolute = |rel: &[u8]| -> Vec<u8> {
            let mut out = fresh_b.to_vec();
            out.push(b'/');
            out.extend_from_slice(rel);
            out
        };
        let mut c: Vec<Vec<u8>> = changed
            .iter()
            .filter(|r| keep(r))
            .map(|r| absolute(r))
            .collect();
        let mut d: Vec<Vec<u8>> = deleted
            .iter()
            .filter(|r| keep(r))
            .map(|r| absolute(r))
            .collect();
        c.sort();
        d.sort();
        (c, d)
    }

    fn write_lists(
        &self,
        target: &str,
        changed_abs: &[Vec<u8>],
        deleted_abs: &[Vec<u8>],
    ) -> Result<(PathBuf, PathBuf), String> {
        let name = if target == "." {
            "root".to_string()
        } else {
            target.replace('/', "_")
        };
        let cf = self.lists_dir.join(format!("{name}.changed"));
        let df = self.lists_dir.join(format!("{name}.deleted"));
        write_list_file(&cf, changed_abs)?;
        write_list_file(&df, deleted_abs)?;
        Ok((cf, df))
    }

    fn step4_patch_backups(
        &mut self,
        changed: &HashSet<Vec<u8>>,
        deleted: &HashSet<Vec<u8>>,
    ) -> Result<(), String> {
        maybe_kill("4");
        log("STEP 4: patch backups");
        let t0 = Instant::now();
        for target in &self.targets.clone() {
            let (c_abs, d_abs) = self.filter_lists(target, changed, deleted);
            let (cf, df) = self.write_lists(target, &c_abs, &d_abs)?;
            let p = self.restic_backup(&self.target_path(target), Some((&cf, &df)))?;
            let out = format!("{}{}", p.stdout, p.stderr);
            if p.rc != 0 {
                return Err(format!("patch backup of {target} failed: {out}"));
            }
            if !out.contains("patching the parent snapshot") {
                let lines: Vec<&str> = out
                    .lines()
                    .filter(|l| l.contains("patch mode"))
                    .collect();
                return Err(format!(
                    "patch backup of {target} declined to a full walk: {lines:?}"
                ));
            }
            log(&format!(
                "STEP 4: target {target}: patch backup engaged (changed={} deleted={})",
                c_abs.len(),
                d_abs.len()
            ));
        }
        self.backup_s = t0.elapsed().as_secs_f64();
        self.write_phase("backed-up", Some(&self.fresh));
        Ok(())
    }

    fn plain_backups(&self, why: &str) -> Result<(), String> {
        log(&format!("plain backups ({why})"));
        for target in &self.targets {
            let p = self.restic_backup(&self.target_path(target), None)?;
            if p.rc != 0 {
                return Err(format!(
                    "plain backup of {target} failed: {}{}",
                    p.stdout, p.stderr
                ));
            }
            log(&format!("plain backup of target {target} done"));
        }
        Ok(())
    }

    fn step5_rotate(&mut self) -> Result<(), String> {
        maybe_kill("5");
        log("STEP 5: rotate retained");
        self.delete_subvolume(&self.retained)?;
        maybe_kill("rotate-mid");
        fs::rename(&self.fresh, &self.retained)
            .map_err(|e| format!("rotate fresh -> retained: {e}"))?;
        self.write_phase("idle", Some(&self.retained));
        Ok(())
    }

    fn rebase_retained(&self) -> Result<(), String> {
        // Fallback path: retained := fresh, the state whose plain
        // backup just succeeded. Also heals a corrupt retained.
        self.remove_any(&self.retained)?;
        fs::rename(&self.fresh, &self.retained)
            .map_err(|e| format!("re-base fresh -> retained: {e}"))?;
        self.write_phase("idle", Some(&self.retained));
        log("retained re-based to the just-backed-up state");
        Ok(())
    }

    fn timings_json(&self, total_s: f64) -> String {
        format!(
            "{{\"snapshot_s\": {}, \"diff_s\": {}, \"backup_s\": {}, \"total_s\": {}}}",
            self.snapshot_s, self.diff_s, self.backup_s, total_s
        )
    }

    // -- the full cycle (producer.py's run()) --------------------------
    pub fn run(&mut self) -> i32 {
        let t_start = Instant::now();
        if let Err(e) = fs::create_dir_all(&self.state) {
            eprintln!("{e}");
            return 1;
        }
        if let Err(e) = self.step1_sweep() {
            eprintln!("{e}");
            return 1;
        }
        if let Err(e) = self.step2_snapshot() {
            eprintln!("{e}");
            return 1;
        }
        if !self.retained.exists() {
            log("INIT: no retained snapshot; plain backup, then retained := fresh");
            if let Err(e) = self.plain_backups("initialisation") {
                log(&format!("HARD FAILURE: {e}"));
                return 1;
            }
            if let Err(e) = fs::rename(&self.fresh, &self.retained) {
                log(&format!("HARD FAILURE: {e}"));
                return 1;
            }
            self.write_phase("idle", Some(&self.retained));
            log("INIT: retained established");
            log(&format!(
                "TIMINGS {}",
                self.timings_json(t_start.elapsed().as_secs_f64())
            ));
            return 0;
        }
        let cycle = self
            .step3_diff()
            .and_then(|(c, d)| self.step4_patch_backups(&c, &d));
        match cycle {
            Err(e) => {
                log(&format!("FALLBACK: {e}"));
                if let Err(e2) = self.plain_backups("producer failure") {
                    log(&format!("HARD FAILURE: {e2}"));
                    return 1;
                }
                if let Err(e2) = self.rebase_retained() {
                    log(&format!("HARD FAILURE: {e2}"));
                    return 1;
                }
                log(&format!(
                    "TIMINGS {}",
                    self.timings_json(t_start.elapsed().as_secs_f64())
                ));
                log("RESULT fallback (retained re-based, not rotated past the backup)");
                3
            }
            Ok(()) => {
                if let Err(e) = self.step5_rotate() {
                    eprintln!("{e}");
                    return 1;
                }
                log(&format!(
                    "TIMINGS {}",
                    self.timings_json(t_start.elapsed().as_secs_f64())
                ));
                log("RESULT patch");
                0
            }
        }
    }

    // -- prepare (wrapper hook 1) ---------------------------------------
    pub fn prepare(&mut self) -> i32 {
        // Filesystem guard, before any state is touched: the
        // lifecycle needs btrfs snapshots of the root. On any other
        // filesystem, decline cleanly with the dedicated exit code
        // 4 and an "unsupported" manifest, so the wrapper falls
        // back to stock backups without the hard-failure path and
        // no state changes happen here (no snapshot, no phase
        // write, no retained/fresh change). A failed statfs falls
        // through to the normal path, which fails there as before.
        if let Some(magic) = fs_magic(&self.root) {
            if magic != BTRFS_SUPER_MAGIC {
                println!(
                    "{{\"mode\": \"unsupported\", \"reason\": \"root filesystem is not btrfs\"}}"
                );
                return 4;
            }
        }
        if let Err(e) = fs::create_dir_all(&self.state) {
            eprintln!("{e}");
            return 1;
        }
        if let Err(e) = self.step1_sweep() {
            eprintln!("{e}");
            return 1;
        }
        if let Err(e) = self.step2_snapshot() {
            eprintln!("{e}");
            return 1;
        }
        let fresh_s = self.fresh.to_string_lossy().into_owned();
        if !self.retained.exists() {
            self.write_phase("prepared", Some(&self.fresh));
            println!(
                "{{\"mode\": \"init\", \"fresh\": \"{}\", \"snapshot_s\": {}}}",
                json_escape(&fresh_s),
                self.snapshot_s
            );
            return 2;
        }
        match self.step3_diff() {
            Err(e) => {
                println!(
                    "{{\"mode\": \"fallback\", \"fresh\": \"{}\", \"reason\": \"{}\"}}",
                    json_escape(&fresh_s),
                    json_escape(&e)
                );
                3
            }
            Ok((changed, deleted)) => {
                let mut targets_json = String::new();
                for (i, target) in self.targets.clone().iter().enumerate() {
                    let (c_abs, d_abs) = self.filter_lists(target, &changed, &deleted);
                    match self.write_lists(target, &c_abs, &d_abs) {
                        Ok((cf, df)) => {
                            if i > 0 {
                                targets_json.push_str(", ");
                            }
                            targets_json.push_str(&format!(
                                "\"{}\": {{\"changed_file\": \"{}\", \"deleted_file\": \"{}\", \"changed\": {}, \"deleted\": {}}}",
                                json_escape(target),
                                json_escape(&cf.to_string_lossy()),
                                json_escape(&df.to_string_lossy()),
                                c_abs.len(),
                                d_abs.len()
                            ));
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            return 1;
                        }
                    }
                }
                self.write_phase("prepared", Some(&self.fresh));
                println!(
                    "{{\"mode\": \"patch\", \"fresh\": \"{}\", \"changed\": {}, \"deleted\": {}, \"snapshot_s\": {}, \"diff_s\": {}, \"targets\": {{{}}}}}",
                    json_escape(&fresh_s),
                    changed.len(),
                    deleted.len(),
                    self.snapshot_s,
                    self.diff_s,
                    targets_json
                );
                0
            }
        }
    }

    // -- finalize (wrapper hook 2) ---------------------------------------
    pub fn finalize(&mut self, result: &str) -> i32 {
        match result {
            "success" => {
                // Mark backed-up BEFORE rotating: a crash inside
                // finalize then leaves a marker the next prepare's
                // step 1 adopts, exactly like a crash before the
                // prototype's rotation.
                if !self.fresh.exists() {
                    eprintln!("finalize --result success: no fresh snapshot to rotate");
                    return 1;
                }
                self.write_phase("backed-up", Some(&self.fresh));
                if let Err(e) = self.delete_subvolume(&self.retained) {
                    eprintln!("{e}");
                    return 1;
                }
                if let Err(e) = fs::rename(&self.fresh, &self.retained) {
                    eprintln!("{e}");
                    return 1;
                }
                self.write_phase("idle", Some(&self.retained));
                log("finalize: retained rotated to the backed-up state");
                0
            }
            "fallback" => match self.rebase_retained() {
                Ok(()) => {
                    log("finalize: retained re-based to the backed-up state");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            },
            "init" => {
                if self.retained.exists() {
                    eprintln!("finalize --result init: retained already exists");
                    return 1;
                }
                if let Err(e) = fs::rename(&self.fresh, &self.retained) {
                    eprintln!("{e}");
                    return 1;
                }
                self.write_phase("idle", Some(&self.retained));
                log("finalize: retained established");
                0
            }
            "abort" => {
                log("finalize: aborted; retained and fresh left as they are");
                0
            }
            other => {
                eprintln!("finalize: unknown --result {other}");
                2
            }
        }
    }
}

fn write_list_file(path: &Path, lines: &[Vec<u8>]) -> Result<(), String> {
    let mut f = fs::File::create(path).map_err(|e| format!("write {}: {e}", path.display()))?;
    for line in lines {
        f.write_all(line).map_err(|e| e.to_string())?;
        f.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// os.path.abspath-style normalisation: resolve "." and ".."
/// lexically (the paths involved exist or are about to; no symlink
/// resolution, matching the prototype).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

pub fn escape_json_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(b as char),
            _ => out.push_str(&format!("\\u{b:04x}")),
        }
    }
    out
}
