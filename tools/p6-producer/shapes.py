#!/usr/bin/env python3
"""Project 6 Phase A: benchmark shapes at corpus scale.

Same differential as run_phaseA (treediff ground truth with content
hashing vs the send-stream parse), run over the 200k-file corpus
(/data/corpus, copied with reflinks so the original stays intact)
for the two shapes used by the earlier enumeration benchmarks:

  spread       ~2,400 changes on distinct paths scattered across
               the tree (modify-append, chmod, rename, delete,
               create).
  concentrated 100,000 appends onto 100 files: many writes, very
               few distinct changed paths.

Ground-truth scoping: the full walk diff with SHA-256 content
hashing is exact for every change class used here (content,
metadata, path set). Each shape runs against its own retained/
fresh snapshot pair chained from the previous shape's result.
"""
import argparse
import os
import random
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sendparse
import treediff


def sh(*argv, **kw):
    subprocess.run(argv, check=True, **kw)


def snapshot(src, dst, ro=True):
    args = ["sudo", "btrfs", "subvolume", "snapshot"]
    if ro:
        args.append("-r")
    args += [src, dst]
    sh(*args, stdout=subprocess.DEVNULL)


def delete_subvol(path):
    sh("sudo", "btrfs", "subvolume", "delete", path,
       stdout=subprocess.DEVNULL)


def inventory(root):
    files = []
    for dirpath, _dirnames, filenames in os.walk(root):
        for fn in filenames:
            files.append(os.path.relpath(
                os.path.join(dirpath, fn), root))
    return sorted(files)


def apply_spread(root, files, rng, total=2400):
    picks = rng.sample(files, total - 360)
    ops = []
    for i, rel in enumerate(picks):
        p = os.path.join(root, rel)
        kind = i % 17
        if kind < 7:            # modify-append
            with open(p, "ab") as fh:
                fh.write(b"S" * 512)
            ops.append(("append", rel))
        elif kind < 10:         # chmod
            os.chmod(p, 0o600 if (i % 2) else 0o640)
            ops.append(("chmod", rel))
        elif kind < 13:         # rename in place
            dst = p + ".mv"
            os.rename(p, dst)
            ops.append(("rename", rel))
        elif kind < 15:         # delete
            os.unlink(p)
            ops.append(("delete", rel))
        else:                   # touch mtime only
            st = os.stat(p)
            os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns + 10**9))
            ops.append(("utimes", rel))
    for j in range(360):        # creates in sampled directories
        d = os.path.dirname(rng.choice(files))
        rel = os.path.join(d, "spread-new-%04d.bin" % j)
        with open(os.path.join(root, rel), "wb") as fh:
            fh.write(os.urandom(2048))
        ops.append(("create", rel))
    return ops


def apply_concentrated(root, files, rng, nfiles=100, appends=1000):
    picks = rng.sample(files, nfiles)
    payload = b"C" * 256
    for rel in picks:
        p = os.path.join(root, rel)
        with open(p, "ab") as fh:
            for _ in range(appends):
                fh.write(payload)
    return [("append-x%d" % appends, rel) for rel in picks]


def run_shape(label, root, retained, fresh, work, apply_fn):
    t0 = time.time()
    old = treediff.walk(retained, hash_content=True)
    t_old = time.time() - t0
    files = inventory(root)
    ops = apply_fn(root, files)
    t_ops = time.time() - t0 - t_old
    text, timings = sendparse.produce(root, retained, fresh, work)
    t1 = time.time()
    new = treediff.walk(fresh, hash_content=True)
    t_new = time.time() - t1
    gt_c, gt_d, _detail = treediff.diff(old, new)
    t2 = time.time()
    p_c, p_d, events = sendparse.parse_dump(text, "fresh", old, new)
    t_parse = time.time() - t2
    om_c = sorted(gt_c - p_c)
    om_d = sorted(gt_d - p_d)
    ex_c = [p for p in sorted(p_c - gt_c)
            if not treediff.only_atime_ctime(old, new, p)]
    ex_d = sorted(p_d - gt_d)
    # Same gate as run_phaseA: the set comparison decides; parser
    # events are printed for the record.
    ok = not om_c and not om_d and not ex_c and not ex_d
    print("[%s] %s: ops=%d gt +%d/-%d producer +%d/-%d omC=%d omD=%d "
          "unexplained=%d exD=%d events=%d" % (
              "PASS" if ok else "FAIL", label, len(ops), len(gt_c),
              len(gt_d), len(p_c), len(p_d), len(om_c), len(om_d),
              len(ex_c), len(ex_d), len(events)))
    print("    walk_old=%.1fs ops=%.1fs send=%s walk_new=%.1fs "
          "parse=%.2fs" % (t_old, t_ops, timings, t_new, t_parse))
    for p in om_c[:5]:
        print("    omit_changed:", p)
    for p in om_d[:5]:
        print("    omit_deleted:", p)
    for p in ex_c[:5]:
        print("    unexplained extra:", p)
    for e in events[:5]:
        print("    event:", e)
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="/data/p6/bench")
    ap.add_argument("--corpus", default="/data/corpus")
    ap.add_argument("--setup", action="store_true",
                    help="create the bench root from the corpus")
    args = ap.parse_args()

    root = os.path.join(args.base, "root")
    retained = os.path.join(args.base, "retained")
    fresh = os.path.join(args.base, "fresh")
    work = os.path.join(args.base, "work")
    if args.setup:
        os.makedirs(args.base, exist_ok=True)
        sh("sudo", "btrfs", "subvolume", "create", root)
        sh("cp", "-a", "--reflink=always",
           args.corpus + "/.", root + "/")
        snapshot(root, retained)
    os.makedirs(work, exist_ok=True)

    rng = random.Random(4242)
    ok = True
    ok &= run_shape("spread", root, retained, fresh, work,
                    lambda r, f: apply_spread(r, f, rng))
    # chain: retained := verified fresh state
    delete_subvol(retained)
    snapshot(fresh, retained)
    delete_subvol(fresh)
    ok &= run_shape("concentrated", root, retained, fresh, work,
                    lambda r, f: apply_concentrated(r, f, rng))
    print("shapes:", "ALL PASS" if ok else "FAILURES PRESENT")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
