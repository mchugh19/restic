#!/usr/bin/env python3
"""Phase A differential harness for Project 6.

For each iteration: mutate the live tree, run the send-stream
producer, compare its changed/deleted lists against the independent
ground-truth tree diff (treediff), and classify every divergence:

  OMISSION      ground truth says changed/deleted, producer does not
                name it. Never acceptable.
  EXTRA-EXPLAINED  producer names a path ground truth does not, and
                the two records for that path differ only in
                atime/ctime (fields restic does not archive by
                default). Demonstrably harmless.
  EXTRA         any other producer-only path. A parser defect until
                explained.
  EVENT         parser findings (unknown stream commands, ambiguous
                dest= splits, unresolved op paths). Unknown commands
                fail the iteration: the parser's vocabulary must be
                complete for the streams these filesystems produce.

Verdict per iteration: PASS iff no OMISSION, no EXTRA, and no
unknown-command events. The Phase A gate is every iteration PASS.
"""

import argparse
import json
import os
import random
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mutate
import sendparse
import treediff


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
    """retained := fresh (as a fresh read-only snapshot of it), so the
    next cycle diffs against the state just verified."""
    _run(["sudo", "btrfs", "subvolume", "delete", retained])
    _run(["sudo", "btrfs", "subvolume", "snapshot", "-r", fresh,
          retained])
    _run(["sudo", "btrfs", "subvolume", "delete", fresh])
    _run(["sync"])


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
    dump_text, timings = sendparse.produce(root, retained, fresh, work)
    safe = label.replace(":", "_").replace("/", "_")
    with open(os.path.join(work, safe + ".dump"), "w",
              encoding="utf-8", errors="surrogateescape") as f:
        f.write(dump_text)
    new = treediff.walk(fresh, hash_content=True)
    gt_c, gt_d, _detail = treediff.diff(old, new)
    p_c, p_d, events = sendparse.parse_dump(
        dump_text, os.path.basename(fresh), old, new)
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
            f"send={res['timings']['send_s']:.2f}s")
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
    ap.add_argument("--workdir", default="/data/p6/phaseA")
    ap.add_argument("--mode", choices=["matrix", "random", "all"],
                    default="all")
    ap.add_argument("--batches", type=int, default=25)
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
