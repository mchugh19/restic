#!/usr/bin/env python3
"""Phase D Gate 1, corpus shapes: the Phase A shapes differential
(spread, concentrated) plus the full-rewrite shape whose parse
OOM-killed the prototype text parser, with the Rust producer
substituted and its peak RSS measured per shape.

Same ground truth and gate as shapes.py (treediff with content
hashing; PASS iff zero omissions, zero unexplained extras, zero
extra deletions). Run as ROOT on the testbed (direct btrfs, direct
parse invocation under /usr/bin/time -v for the RSS figure).

The full-rewrite shape rewrites every file's content at the same
size (the bench_phaseC.py mutation): the send stream names every
file, the stream is tens of megabytes, and the parser's memory
must stay bounded by structure, not by stream size.
"""
import argparse
import os
import random
import re
import subprocess
import sys
import time

sys.path.insert(0, "/home/ubuntu/p6-producer")
import treediff

RS_BIN = os.environ.get("P6_RS_BIN", "/home/ubuntu/p6-producer-rs-bin")


def sh(*argv, **kw):
    subprocess.run(argv, check=True, **kw)


def snapshot(src, dst):
    if os.path.exists(dst):
        sh("btrfs", "subvolume", "delete", dst, stdout=subprocess.DEVNULL)
    sh("btrfs", "subvolume", "snapshot", "-r", src, dst,
       stdout=subprocess.DEVNULL)
    sh("sync")


def delete_subvol(path):
    if os.path.exists(path):
        sh("btrfs", "subvolume", "delete", path, stdout=subprocess.DEVNULL)


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
        if kind < 7:
            with open(p, "ab") as fh:
                fh.write(b"S" * 512)
            ops.append(("append", rel))
        elif kind < 10:
            os.chmod(p, 0o600 if (i % 2) else 0o640)
            ops.append(("chmod", rel))
        elif kind < 13:
            os.rename(p, p + ".mv")
            ops.append(("rename", rel))
        elif kind < 15:
            os.unlink(p)
            ops.append(("delete", rel))
        else:
            st = os.stat(p)
            os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns + 10**9))
            ops.append(("utimes", rel))
    for j in range(360):
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


def apply_full_rewrite(root, files, rng):
    n = 0
    for rel in files:
        p = os.path.join(root, rel)
        size = os.path.getsize(p)
        buf = rng.randbytes(8192)
        with open(p, "wb") as fh:
            written = 0
            while written < size:
                chunk = buf[:min(8192, size - written)]
                fh.write(chunk)
                written += len(chunk)
        n += 1
    return [("rewrite-all", n)]


def read_set(path):
    data = open(path, "rb").read()
    if not data:
        return set()
    lines = data.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    return set(x.decode("utf-8", "surrogateescape") for x in lines)


def produce_rs(root, retained, fresh, work):
    os.makedirs(work, exist_ok=True)
    stream = os.path.join(work, "stream.bin")
    changed_f = os.path.join(work, "rs.changed")
    deleted_f = os.path.join(work, "rs.deleted")
    t0 = time.time()
    snapshot(root, fresh)
    t1 = time.time()
    with open(stream, "wb") as f:
        subprocess.run(["btrfs", "send", "--no-data", "-p", retained, fresh],
                       check=True, stdout=f, stderr=subprocess.PIPE)
    t2 = time.time()
    p = subprocess.run(["/usr/bin/time", "-v", RS_BIN, "parse",
                        "--stream", stream, "--retained", retained,
                        "--fresh", fresh, "--changed-out", changed_f,
                        "--deleted-out", deleted_f],
                       capture_output=True, text=True)
    t3 = time.time()
    if p.returncode != 0:
        raise RuntimeError(f"rust parse failed rc={p.returncode}: "
                           f"{p.stderr.strip()[-500:]}")
    m = re.search(r"Maximum resident set size \(kbytes\): (\d+)",
                  p.stderr)
    rss_kb = int(m.group(1)) if m else -1
    timings = {"snapshot_s": round(t1 - t0, 2), "send_s": round(t2 - t1, 2),
               "parse_s": round(t3 - t2, 2),
               "stream_bytes": os.path.getsize(stream),
               "parse_rss_kb": rss_kb}
    return read_set(changed_f), read_set(deleted_f), timings


def run_shape(label, root, retained, fresh, work, apply_fn):
    t0 = time.time()
    old = treediff.walk(retained, hash_content=True)
    t_old = time.time() - t0
    files = inventory(root)
    ops = apply_fn(root, files)
    t_ops = time.time() - t0 - t_old
    p_c, p_d, timings = produce_rs(root, retained, fresh, work)
    t1 = time.time()
    new = treediff.walk(fresh, hash_content=True)
    t_new = time.time() - t1
    gt_c, gt_d, _detail = treediff.diff(old, new)
    om_c = sorted(gt_c - p_c)
    om_d = sorted(gt_d - p_d)
    ex_c = [p for p in sorted(p_c - gt_c)
            if not treediff.only_atime_ctime(old, new, p)]
    ex_d = sorted(p_d - gt_d)
    ok = not om_c and not om_d and not ex_c and not ex_d
    print("[%s] %s: ops=%d gt +%d/-%d producer +%d/-%d omC=%d omD=%d "
          "unexplained=%d exD=%d" % (
              "PASS" if ok else "FAIL", label, len(ops), len(gt_c),
              len(gt_d), len(p_c), len(p_d), len(om_c), len(om_d),
              len(ex_c), len(ex_d)), flush=True)
    print("    walk_old=%.1fs ops=%.1fs %s walk_new=%.1fs" % (
        t_old, t_ops, timings, t_new), flush=True)
    for p in om_c[:5]:
        print("    omit_changed:", p)
    for p in om_d[:5]:
        print("    omit_deleted:", p)
    for p in ex_c[:5]:
        print("    unexplained extra:", p)
    return ok


def chain(retained, fresh):
    delete_subvol(retained)
    snapshot(fresh, retained)
    delete_subvol(fresh)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="/data/p6/benchD")
    ap.add_argument("--corpus", default="/data/corpus")
    ap.add_argument("--setup", action="store_true")
    ap.add_argument("--shapes", default="spread,concentrated,full-rewrite")
    args = ap.parse_args()

    root = os.path.join(args.base, "root")
    retained = os.path.join(args.base, "retained")
    fresh = os.path.join(args.base, "fresh")
    work = os.path.join(args.base, "work")
    if args.setup:
        os.makedirs(args.base, exist_ok=True)
        delete_subvol(root)
        sh("btrfs", "subvolume", "create", root)
        sh("cp", "-a", "--reflink=always",
           args.corpus + "/.", root + "/")
        snapshot(root, retained)
    os.makedirs(work, exist_ok=True)

    rng = random.Random(4242)
    shape_fns = {
        "spread": lambda r, f: apply_spread(r, f, rng),
        "concentrated": lambda r, f: apply_concentrated(r, f, rng),
        "full-rewrite": lambda r, f: apply_full_rewrite(r, f, rng),
    }
    ok = True
    for label in args.shapes.split(","):
        ok &= run_shape(label, root, retained, fresh, work,
                        shape_fns[label])
        chain(retained, fresh)
    print("shapes:", "ALL PASS" if ok else "FAILURES PRESENT")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
