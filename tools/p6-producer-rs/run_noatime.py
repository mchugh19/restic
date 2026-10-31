#!/usr/bin/env python3
"""Gate (a) for the atime-only filter: the Phase A differential
under the tightened verdict the filter makes possible.

Ground truth is treediff, as before, but each path is now
classified three ways from the two metadata walks:

  substantive   treediff.diff's changed set (type, mode, uid, gid,
                mtime_ns, size, target, xattrs, content hash and
                nlink all compared; atime/ctime are NOT change
                keys, so a path here changed in a way restic must
                hear about)
  atime-class   present in both trees, not substantive, and the
                records differ only in atime_ns/ctime_ns
  deleted       present only in the retained tree

PASS iff producer changed == GT substantive EXACTLY (no extras,
no omissions) and producer deleted == GT deleted. Before the
filter, producer extras in this differential were tolerated when
treediff classified them atime/ctime-only (256 of 256 historical
extras were exactly that); the filter exists to make them not
happen, so the tolerance is gone.

The producer's own atime_only count (from the parse summary) is
reported beside the GT atime-class size as a cross-check; it is
diagnostic, not part of the verdict.

Run as the ubuntu user on the testbed, from a directory where
mutate.py and treediff.py resolve (the runner inserts
/home/ubuntu/p6-producer, as run_phaseD.py does). P6_RS_BIN
selects the producer binary.
"""
import argparse
import json
import os
import random
import shutil
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


def produce_rs(root, retained, fresh, work, env_extra=None):
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
    # env_extra goes through `sudo env`: sudo's env_reset would
    # otherwise strip drill-hook variables (P6_NO_CTIME_PROOF)
    # before the producer ever saw them.
    argv = ["sudo"]
    if env_extra:
        argv += ["env"] + [f"{k}={v}" for k, v in env_extra.items()]
    argv += [RS_BIN, "parse", "--stream", stream,
             "--retained", retained, "--fresh", fresh,
             "--changed-out", changed_f,
             "--deleted-out", deleted_f]
    p = subprocess.run(argv, capture_output=True, text=True)
    t3 = time.time()
    if p.returncode != 0:
        raise RuntimeError(f"rust parse failed rc={p.returncode}: "
                           f"{p.stderr.strip()}")
    summary = json.loads(p.stdout.strip().splitlines()[-1])
    timings = {"snapshot_s": t1 - t0, "send_s": t2 - t1,
               "parse_s": t3 - t2,
               "stream_bytes": os.path.getsize(stream)}
    return (read_set(changed_f), read_set(deleted_f),
            summary["events"], summary.get("atime_only", -1), timings)


def gt_atime_class(old, new, gt_c):
    """Paths in both trees, not substantive, differing only in
    atime_ns/ctime_ns."""
    out = set()
    for p in old:
        if p in new and p not in gt_c \
                and treediff.only_atime_ctime(old, new, p):
            out.add(p)
    return out


def cycle(case_dir, root, retained, fresh, label, apply_fn,
          env_extra=None):
    work = os.path.join(case_dir, "work")
    old = treediff.walk(retained, hash_content=True)
    oplog = []
    apply_fn(oplog)
    t0 = time.time()
    p_c, p_d, events, ao, timings = produce_rs(root, retained, fresh,
                                               work, env_extra)
    new = treediff.walk(fresh, hash_content=True)
    gt_c, gt_d, _detail = treediff.diff(old, new)
    gt_ao = gt_atime_class(old, new, gt_c)
    missing_c = sorted(gt_c - p_c)
    extra_c = sorted(p_c - gt_c)
    missing_d = sorted(gt_d - p_d)
    extra_d = sorted(p_d - gt_d)
    unknown = [e for e in events if e["kind"] == "unknown-command"]
    verdict = not (missing_c or extra_c or missing_d or extra_d
                   or unknown)
    res = {
        "verdict": "PASS" if verdict else "FAIL",
        "label": label,
        "ops": oplog,
        "gt_changed": len(gt_c), "gt_deleted": len(gt_d),
        "gt_atime_class": len(gt_ao),
        "p_changed": len(p_c), "p_deleted": len(p_d),
        "p_atime_only": ao,
        "omissions_changed": missing_c, "extras_changed": extra_c,
        "omissions_deleted": missing_d, "extras_deleted": extra_d,
        "unknown_commands": unknown,
        "events": events,
        "p_changed_list": sorted(p_c),
        "timings": timings, "cycle_wall_s": round(time.time() - t0, 2),
    }
    chain(case_dir, root, retained, fresh)
    return res


def emit(res, out):
    line = (f"[{res['verdict']}] {res['label']}: "
            f"gt +{res['gt_changed']}/-{res['gt_deleted']} "
            f"producer +{res['p_changed']}/-{res['p_deleted']} "
            f"gtAO={res['gt_atime_class']} pAO={res['p_atime_only']} "
            f"omC={len(res['omissions_changed'])} "
            f"exC={len(res['extras_changed'])} "
            f"omD={len(res['omissions_deleted'])} "
            f"exD={len(res['extras_deleted'])} "
            f"unk={len(res['unknown_commands'])} "
            f"send={res['timings']['send_s']:.2f}s "
            f"parse={res['timings']['parse_s']:.2f}s")
    print(line, flush=True)
    if res["verdict"] == "FAIL":
        for k in ("omissions_changed", "extras_changed",
                  "omissions_deleted", "extras_deleted"):
            for p in res[k][:10]:
                print(f"    {k}: {p}", flush=True)
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
                   apply_fn), out)


def run_random(base, out, batches, ops_per_batch, seed0=777):
    case = os.path.join(base, "random")
    root, retained, fresh = setup_case(
        case, lambda r, s: mutate.build_random_tree(r, s), seed0)
    total_ops = 0
    for b in range(batches):
        rng = random.Random(seed0 * 1000 + b)

        def apply_fn(oplog, rng=rng):
            mutate.apply_random_batch(root, rng, ops_per_batch, oplog)
        res = cycle(case, root, retained, fresh,
                    f"random:batch{b}", apply_fn)
        total_ops += len([o for o in res["ops"]
                          if not o[0].startswith("op-failed")])
        res["cumulative_ops"] = total_ops
        emit(res, out)
    print(f"random phase complete: {total_ops} ops applied", flush=True)


# ----------------------------------------------------------------------
# The v6 ctime traps. Five dedicated cycles pinning the ctime
# proof's semantics against the tightened ground truth AND against
# the producer's own diagnostic events:
#   1. a read sweep over pre-retained files: every drop must be
#      carried by the ctime proof (aggregate event == dropped);
#   2. an mtime-only touch inside the interval: ctime is inside
#      the interval, the stat proof keeps it (mtime moved);
#   3. a deliberate utimensat atime-only set inside the interval:
#      dropped by the stat proof, ctime aggregate stays 0;
#   4. a chmod inside the interval: substantive op, kept, and not
#      a candidate at all;
#   5. the same read sweep with P6_NO_CTIME_PROOF=1: the verdict
#      must equal the enabled run's exactly, with the
#      ctime-proof-disabled event present instead.
# The fixture files were created before the retained snapshot, so
# their ctimes predate the threshold: the baseline read sweep in
# cycle 1 exercises the proof the way production does.

def ctime_event(events, kind):
    for e in events:
        if e.get("kind") == kind:
            return e.get("path")
    return None


def ctime_traps(workdir, out):
    case = os.path.join(workdir, "ctime-traps")
    root = os.path.join(case, "root")
    retained = os.path.join(case, "retained")
    fresh = os.path.join(case, "fresh")
    if os.path.isdir(case):
        # Remove every subvolume in the case dir (read-only
        # snapshots defeat plain rmtree), then the husk.
        for name in os.listdir(case):
            p = os.path.join(case, name)
            if os.path.isdir(p):
                subprocess.run(["sudo", "btrfs", "subvolume",
                                "delete", p], capture_output=True)
        _run(["sudo", "rm", "-rf", case])
    os.makedirs(case)
    _run(["sudo", "btrfs", "subvolume", "create", root])
    _run(["sudo", "chown", "ubuntu:ubuntu", root])
    for d in range(40):
        dp = os.path.join(root, f"dir{d:02d}")
        os.makedirs(dp)
        for f in range(25):
            p = os.path.join(dp, f"f{f:02d}.txt")
            with open(p, "w") as fh:
                fh.write(f"trap content {d}/{f}\n" * 3)
    sh_snapshot(root, retained)

    def make_sweep(lo, hi):
        def sweep_reads(oplog):
            n = 0
            for d in range(lo, hi):
                dp = os.path.join(root, f"dir{d:02d}")
                for name in sorted(os.listdir(dp)):
                    with open(os.path.join(dp, name), "rb") as fh:
                        fh.read(1)
                    n += 1
            oplog.append(f"read-sweep dirs {lo}..{hi - 1} x{n}")
        return sweep_reads

    # First half of the tree, proof enabled.
    res = cycle(case, root, retained, fresh, "ctime:read-sweep",
                make_sweep(0, 20))
    assert res["verdict"] == "PASS", res
    assert res["p_atime_only"] > 0, res
    assert ctime_event(res["events"], "ctime-proof") == \
        str(res["p_atime_only"]), res["events"]
    res["trap_asserts"] = {"all_drops_ctime_proven": True}
    emit(res, out)

    def mtime_touch(oplog):
        p = os.path.join(root, "dir03", "f07.txt")
        st = os.lstat(p)
        os.utime(p, ns=(st.st_atime_ns,
                        st.st_mtime_ns + 3_000_000_000_000))
        oplog.append("mtime-only touch (ctime inside interval)")

    res = cycle(case, root, retained, fresh, "ctime:mtime-touch",
                mtime_touch)
    assert res["verdict"] == "PASS", res
    assert "dir03/f07.txt" in res["p_changed_list"], res
    assert ctime_event(res["events"], "ctime-proof") == "0"
    res["trap_asserts"] = {"kept_by_stat": True, "ctime_proven": "0"}
    emit(res, out)

    def utimensat_atime(oplog):
        p = os.path.join(root, "dir05", "f11.txt")
        st = os.lstat(p)
        os.utime(p, ns=(time.time_ns(), st.st_mtime_ns))
        oplog.append("utimensat atime-only (ctime inside interval)")

    res = cycle(case, root, retained, fresh, "ctime:utimensat",
                utimensat_atime)
    assert res["verdict"] == "PASS", res
    assert res["p_atime_only"] >= 1, res
    assert ctime_event(res["events"], "ctime-proof") == "0"
    res["trap_asserts"] = {"dropped_by_stat": True, "ctime_proven": "0"}
    emit(res, out)

    def chmod_trap(oplog):
        p = os.path.join(root, "dir07", "f03.txt")
        os.chmod(p, 0o600)
        oplog.append("chmod (substantive)")

    res = cycle(case, root, retained, fresh, "ctime:chmod", chmod_trap)
    assert res["verdict"] == "PASS", res
    assert "dir07/f03.txt" in res["p_changed_list"], res
    res["trap_asserts"] = {"kept_substantive": True}
    emit(res, out)

    # Second half of the tree (never read before, so its atimes
    # are still stale), proof disabled by the drill hook: the
    # same drops must happen, all by the stat proof, with the
    # disabled event present. The tightened GT verdict already
    # pins the changed set exactly; gt_atime_class is the count
    # of atime-class paths the sweep produced.
    res = cycle(case, root, retained, fresh, "ctime:sweep-disabled",
                make_sweep(20, 40),
                env_extra={"P6_NO_CTIME_PROOF": "1"})
    assert res["verdict"] == "PASS", res
    assert res["p_atime_only"] > 0, res
    assert res["p_atime_only"] == res["gt_atime_class"], res
    # The disabled event carries no path field, so presence is
    # checked by kind, not via ctime_event's return value.
    kinds = [e.get("kind") for e in res["events"]]
    assert "ctime-proof-disabled" in kinds, res["events"]
    assert "ctime-proof" not in kinds, res["events"]
    res["trap_asserts"] = {"disabled_event": True,
                           "drops_equal_gt_atime_class": True}
    emit(res, out)
    print("ctime traps complete", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", default="/data/p6/noatime")
    ap.add_argument("--mode", choices=["matrix", "random", "ctime",
                                       "all"],
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
        if args.mode in ("ctime", "all"):
            ctime_traps(args.workdir, out)
        if args.mode in ("random", "all"):
            run_random(args.workdir, out, args.batches,
                       args.ops_per_batch)
    print(f"results: {results_path}")


if __name__ == "__main__":
    main()
