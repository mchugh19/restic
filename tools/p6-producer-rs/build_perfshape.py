#!/usr/bin/env python3
"""Perf-shape fixture setup (runs ON the OCI testbed).

Builds /data/p6/perfshape: a subvolume whose content is a reflink
copy of the 200k corpus, a retained snapshot, then a full-tree
atime-only sweep (os.utime with a new atime and the mtime preserved
to the nanosecond, files, dirs and symlinks) plus a small set of
substantive changes, then a fresh snapshot and a spooled
`btrfs send --no-data` stream between them. Timings and the
expected substantive sets are recorded as JSON beside the fixture
for run_perfshape.py to consume. Idempotent: rebuilds from scratch.
"""
import json
import os
import subprocess
import sys
import time

BASE = "/data/p6/perfshape"
ROOT = os.path.join(BASE, "root")
RETAINED = os.path.join(BASE, "retained")
FRESH = os.path.join(BASE, "fresh")
CORPUS = "/data/corpus"


def run(cmd, **kw):
    subprocess.run(cmd, check=True, **kw)


def del_subvol(path):
    if os.path.exists(path):
        subprocess.run(["sudo", "btrfs", "subvolume", "delete", path],
                       check=False, capture_output=True)


def snapshot(src, dst):
    del_subvol(dst)
    run(["sudo", "btrfs", "subvolume", "snapshot", "-r", src, dst])
    run(["sync"])


def main():
    t00 = time.time()
    for p in (FRESH, RETAINED, ROOT):
        del_subvol(p)
    os.makedirs(BASE, exist_ok=True)
    run(["sudo", "btrfs", "subvolume", "create", ROOT])
    run(["sudo", "chown", "ubuntu:ubuntu", ROOT])
    t0 = time.time()
    run(["cp", "--reflink=always", "-a", CORPUS + "/.", ROOT + "/"])
    copy_s = time.time() - t0
    snapshot(ROOT, RETAINED)

    # ---- the sweep: atime-only on every entry -------------------
    now_ns = time.time_ns()
    swept = 0
    all_files = []
    all_dirs = []
    for dirpath, dirnames, filenames in os.walk(ROOT):
        for name in dirnames + filenames:
            p = os.path.join(dirpath, name)
            try:
                st = os.lstat(p)
                os.utime(p, ns=(now_ns, st.st_mtime_ns),
                         follow_symlinks=False)
                swept += 1
            except OSError as e:
                print(f"sweep skip {p}: {e}", flush=True)
        for name in filenames:
            all_files.append(os.path.relpath(
                os.path.join(dirpath, name), ROOT))
        all_dirs.append(os.path.relpath(dirpath, ROOT))
    all_files.sort()
    sweep_s = time.time() - t0 - copy_s

    exp_changed = set()
    exp_deleted = set()

    def rel(p):
        return os.path.relpath(p, ROOT)

    # substantive: rewrite content of 5 files
    for f in all_files[0:50:10]:
        p = os.path.join(ROOT, f)
        with open(p, "r+b") as fh:
            fh.write(b"PERFSHAPE-REWRITE")
        exp_changed.add(f)
    # substantive: append to 3 files
    for f in all_files[100:160:20]:
        p = os.path.join(ROOT, f)
        with open(p, "ab") as fh:
            fh.write(b"appended-by-perfshape")
        exp_changed.add(f)
    # substantive trap: mtime-only move on 3 files (filter must keep)
    for f in all_files[200:260:20]:
        p = os.path.join(ROOT, f)
        st = os.lstat(p)
        os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns + 5_000_000_000_000))
        exp_changed.add(f)
    # substantive: chmod 2 files
    for f in all_files[300:340:20]:
        os.chmod(os.path.join(ROOT, f), 0o600)
        exp_changed.add(f)
    # substantive: create a new dir with 3 files
    nd = os.path.join(ROOT, "perf-new")
    os.makedirs(nd, exist_ok=True)
    for i in range(3):
        p = os.path.join(nd, f"new{i}.txt")
        with open(p, "w") as fh:
            fh.write("new file\n")
        exp_changed.add(rel(p))
    # substantive: delete 4 files
    for f in all_files[400:480:20]:
        os.unlink(os.path.join(ROOT, f))
        exp_deleted.add(f)
    # substantive: delete one whole directory (first leaf dir with
    # exactly the files we can enumerate)
    victim_dir = None
    for d in all_dirs:
        if d == ".":
            continue
        full = os.path.join(ROOT, d)
        entries = os.listdir(full)
        if entries and all(os.path.isfile(os.path.join(full, e))
                           for e in entries):
            victim_dir = d
            for e in entries:
                exp_deleted.add(os.path.join(d, e))
            break
    if victim_dir:
        import shutil
        shutil.rmtree(os.path.join(ROOT, victim_dir))
        exp_deleted.add(victim_dir)
    # substantive: rename 2 files
    for i, f in enumerate(all_files[500:540:20]):
        src = os.path.join(ROOT, f)
        dst = src + ".renamed"
        os.rename(src, dst)
        exp_deleted.add(f)
        exp_changed.add(rel(dst))
    # substantive: one extra hardlink to a swept, untouched file
    link_target = all_files[600]
    link_name = link_target + ".hardlink"
    os.link(os.path.join(ROOT, link_target),
            os.path.join(ROOT, link_name))
    exp_changed.add(link_target)
    exp_changed.add(link_name)

    snapshot(ROOT, FRESH)

    # ---- spool the send stream, timed ----------------------------
    stream = os.path.join(BASE, "stream.bin")
    t0 = time.time()
    with open(stream, "wb") as fh:
        run(["sudo", "btrfs", "send", "--no-data", "-p", RETAINED, FRESH],
            stdout=fh)
    send_s = time.time() - t0

    meta = {
        "swept_entries": swept,
        "corpus_files": len(all_files),
        "copy_s": round(copy_s, 2),
        "sweep_s": round(sweep_s, 2),
        "send_s": round(send_s, 2),
        "stream_bytes": os.path.getsize(stream),
        "expected_changed_files": sorted(exp_changed),
        "expected_deleted": sorted(exp_deleted),
        "setup_wall_s": round(time.time() - t00, 2),
    }
    with open(os.path.join(BASE, "expected.json"), "w") as fh:
        json.dump(meta, fh, indent=1)
    print(json.dumps(meta, indent=1), flush=True)
    print("SETUP DONE", flush=True)


if __name__ == "__main__":
    sys.exit(main())
