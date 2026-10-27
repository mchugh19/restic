#!/usr/bin/env python3
"""Phase D Gate 1: the Phase A differential, unchanged, with the
Rust producer (tools/p6-producer-rs) substituted for the prototype
text parser.

Same ground truth (treediff), same classification of divergences,
same verdict rule as run_phaseA.py: PASS iff no OMISSION, no
unexplained EXTRA, no extra deletions, and no parser failure. The
only change is how the producer lists are obtained: snapshot, then
`btrfs send --no-data` to a stream file, then
`p6-producer parse` (streaming TLV) instead of
`btrfs receive --dump` + sendparse.parse_dump.

Run as the ubuntu user on the testbed (btrfs operations and the
parse's INO_PATHS go through sudo), from a directory containing
mutate.py and treediff.py on sys.path.
"""
import argparse
import json
import os
import random
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, "/home/ubuntu/p6-producer")
import mutate
import treediff

RS_BIN = os.environ.get("P6_RS_BIN", "/home/ubuntu/p6-producer-rs-bin")


def _run(cmd):
    subprocess.run(cmd, check=True, capture_output=True, text=True)


def sh_snapshot(src, dst):
    if os.path.exists(dst):
        _run(["sudo", "btrfs", "subvolume", "delete", dst])
    _run(["sudo", "btrfs", "subvolume", "snapshot", "-r", src, dst])
    _run(["sync"])


def setup_case(case_dir, builder, seed):
    os.makedirs(case_dir, exist_ok=True)
    root = os.path.join(case_dir, "root")
    if os.path.exists(root):
        _run(["sudo", "btrfs", "subvolume", "delete", root])
    _run(["sudo", "btrfs", "subvolume", "create", root])
    _run(["sudo", "chown", "ubuntu:ubuntu", root])
    builder(root, seed)
    retained = os.path.join(case_dir, "retained")
    sh_snapshot(root, retained)
    return root, retained, os.path.join(case_dir, "fresh")


def chain(case_dir, root, retained, fresh):
    _run(["sudo", "btrfs", "subvolume", "delete", retained])
    _run(["sudo", "btrfs", "subvolume", "snapshot", "-r", fresh,
          retained])
    _run(["sudo", "btrfs", "subvolume", "delete", fresh])
    _run(["sync"])


def read_set(path):
    data = open(path, "rb").read()
    if not data:
        return set()
    lines = data.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    return set(x.decode("utf-8", "surrogateescape") for x in lines)


def produce_rs(root, retained, fresh, work):
    """Snapshot + send + Rust parse. Returns (changed, deleted,
    events, timings). A parser failure raises: the gate's verdict
    rule counts it as a failed iteration, and one is enough to stop
    on (the Phase A standard was a parser whose vocabulary and
    semantics were complete for these streams)."""
    os.makedirs(work, exist_ok=True)
    stream = os.path.join(work, "stream.bin")
    changed_f = os.path.join(work, "rs.changed")
    deleted_f = os.path.join(work, "rs.deleted")
    t0 = time.time()
    sh_snapshot(root, fresh)
    t1 = time.time()
    with open(stream, "wb") as f:
        subprocess.run(["sudo", "btrfs", "send", "--no-data",
                        "-p", retained, fresh],
                       check=True, stdout=f, stderr=subprocess.PIPE)
    t2 = time.time()
    p = subprocess.run(["sudo", RS_BIN, "parse", "--stream", stream,
                        "--retained", retained, "--fresh", fresh,
                        "--changed-out", changed_f,
                        "--deleted-out", deleted_f],
                       capture_output=True, text=True)
    t3 = time.time()
    if p.returncode != 0:
        raise RuntimeError(f"rust parse failed rc={p.returncode}: "
                           f"{p.stderr.strip()}")
    summary = json.loads(p.stdout.strip().splitlines()[-1])
    timings = {"snapshot_s": t1 - t0, "send_s": t2 - t1,
               "parse_s": t3 - t2,
               "stream_bytes": os.path.getsize(stream)}
    return read_set(changed_f), read_set(deleted_f), summary["events"], timings


def compare(old, new, gt_c, gt_d, p_c, p_d, events):
    missing_c = sorted(gt_c - p_c)
    missing_d = sorted(gt_d - p_d)
    extra_c = []
    for p in sorted(p_c - gt_c):
        if treediff.only_atime_ctime(old, new, p):
            extra_c.append((p, "atime/ctime-only"))
        elif p not in new:
            extra_c.append((p, "not-in-fresh-tree"))
        else:
            extra_c.append((p, "unexplained"))
    extra_d = []
    for p in sorted(p_d - gt_d):
        where = ("in-both-trees" if p in old and p in new else
                 "in-old-only" if p in old else "in-neither")
        extra_d.append((p, where))
    unknown = [e for e in events if e["kind"] == "unknown-command"]
    unexplained = [p for p, why in extra_c if why == "unexplained"]
    verdict = not (missing_c or missing_d or unexplained or extra_d
                   or unknown)
    return {
        "verdict": "PASS" if verdict else "FAIL",
        "omissions_changed": missing_c,
        "omissions_deleted": missing_d,
        "extras_changed": extra_c,
        "extras_deleted": extra_d,
        "unknown_commands": unknown,
        "events": events,
    }


def cycle(case_dir, root, retained, fresh, label, apply_fn, rng):
    work = os.path.join(case_dir, "work")
    old = treediff.walk(retained, hash_content=True)
    oplog = []
    apply_fn(oplog)
    t0 = time.time()
    p_c, p_d, events, timings = produce_rs(root, retained, fresh, work)
    new = treediff.walk(fresh, hash_content=True)
    gt_c, gt_d, _detail = treediff.diff(old, new)
    res = compare(old, new, gt_c, gt_d, p_c, p_d, events)
    res.update({
        "label": label,
        "ops": oplog,
        "gt_changed": len(gt_c), "gt_deleted": len(gt_d),
        "p_changed": len(p_c), "p_deleted": len(p_d),
        "timings": timings, "cycle_wall_s": round(time.time() - t0, 2),
    })
    chain(case_dir, root, retained, fresh)
    return res


def emit(res, out):
    line = (f"[{res['verdict']}] {res['label']}: "
            f"gt +{res['gt_changed']}/-{res['gt_deleted']} "
            f"producer +{res['p_changed']}/-{res['p_deleted']} "
            f"omC={len(res['omissions_changed'])} "
            f"omD={len(res['omissions_deleted'])} "
            f"exC={len(res['extras_changed'])} "
            f"exD={len(res['extras_deleted'])} "
            f"unk={len(res['unknown_commands'])} "
            f"send={res['timings']['send_s']:.2f}s "
            f"parse={res['timings']['parse_s']:.2f}s")
    print(line, flush=True)
    if res["verdict"] == "FAIL":
        for k in ("omissions_changed", "omissions_deleted"):
            for p in res[k][:10]:
                print(f"    {k}: {p}", flush=True)
        for p, why in res["extras_changed"][:10]:
            print(f"    extra_changed[{why}]: {p}", flush=True)
        for p, why in res["extras_deleted"][:10]:
            print(f"    extra_deleted[{why}]: {p}", flush=True)
        for e in res["unknown_commands"][:5]:
            print(f"    unknown: {e}", flush=True)
    out.write(json.dumps(res) + "\n")
    out.flush()


def run_matrix(base, out):
    case = os.path.join(base, "matrix")
    root, retained, fresh = setup_case(
        case, lambda r, s: mutate.build_fixture(r, s), 20261026)
    cases = mutate.matrix_cases()
    for idx, (name, funcs) in enumerate(cases.items()):
        rng = random.Random(1000 + idx)

        def apply_fn(oplog, funcs=funcs, rng=rng):
            for fn in funcs:
                inv = mutate.inventory(root)
                try:
                    fn(root, rng, inv, oplog)
                except (OSError, IndexError, ValueError) as e:
                    oplog.append(("op-failed:" + fn.__name__, repr(e)))
        emit(cycle(case, root, retained, fresh, "matrix:" + name,
                   apply_fn, rng), out)


def run_random(base, out, batches, ops_per_batch, seed0=777):
    case = os.path.join(base, "random")
    root, retained, fresh = setup_case(
        case,
        lambda r, s: mutate.build_random_tree(r, s),
        seed0)
    total_ops = 0
    for b in range(batches):
        rng = random.Random(seed0 * 1000 + b)

        def apply_fn(oplog, rng=rng):
            mutate.apply_random_batch(root, rng, ops_per_batch, oplog)
        res = cycle(case, root, retained, fresh, f"random:batch{b}",
                    apply_fn, rng)
        total_ops += len([o for o in res["ops"]
                          if not o[0].startswith("op-failed")])
        res["cumulative_ops"] = total_ops
        emit(res, out)
    print(f"random phase complete: {total_ops} ops applied", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", default="/data/p6/phaseD")
    ap.add_argument("--mode", choices=["matrix", "random", "all"],
                    default="all")
    ap.add_argument("--batches", type=int, default=40)
    ap.add_argument("--ops-per-batch", type=int, default=150)
    ap.add_argument("--results", default=None)
    args = ap.parse_args()
    os.makedirs(args.workdir, exist_ok=True)
    results_path = args.results or os.path.join(
        args.workdir, f"results-{args.mode}.jsonl")
    with open(results_path, "a") as out:
        if args.mode in ("matrix", "all"):
            run_matrix(args.workdir, out)
        if args.mode in ("random", "all"):
            run_random(args.workdir, out, args.batches,
                       args.ops_per_batch)
    print(f"results: {results_path}")


if __name__ == "__main__":
    main()
