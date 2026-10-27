#!/usr/bin/env python3
"""Project 6 Phase D benchmarks (OCI testbed, 200k corpus): the
Phase C benchmark with the Rust producer binary substituted.
Producer totals are compared against the recorded Phase C
prototype numbers in the phase report.

For each mutation shape applied cumulatively to a reflink copy of
the corpus: producer enumeration time (fresh snapshot + send +
parse, including the prototype parser's metadata walks), patch
backup time, total producer run time, the retained snapshot's
pinned bytes after rotation, and a stock full backup of the same
mutated state (same repo, parent-accelerated, as the production
wrapper runs it) for comparison.

Run as root on the testbed. Writes /home/ubuntu/bench-phaseC.json
and prints one summary block per shape.
"""
import json
import os
import random
import subprocess
import sys
import time

sys.path.insert(0, "/home/ubuntu/p6-producer")
import shapes  # noqa: E402

PATCH_BIN = "/home/ubuntu/restic-patch6"
STOCK_BIN = "/home/ubuntu/restic-stock2"
PRODUCER = "/home/ubuntu/p6-producer-rs-bin"
BASE = "/data/p6/benchD-rs"
ROOT = os.path.join(BASE, "root")
STATE = os.path.join(BASE, "state")
RETAINED = os.path.join(STATE, "retained")
REFSNAP = os.path.join(BASE, "refsnap")
REPO_PATCH = os.path.join(BASE, "repo-patch")
REPO_STOCK = os.path.join(BASE, "repo-stock")
PW = "p6bench-phaseD"
OUT = "/home/ubuntu/bench-phaseD.json"


def run(cmd, env=None, timeout=7200):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(cmd, capture_output=True, text=True, env=e,
                          timeout=timeout)


def must(cmd, env=None):
    p = run(cmd, env=env)
    if p.returncode != 0:
        raise RuntimeError(f"{cmd}\n{p.stdout}\n{p.stderr}")
    return p


def is_subvolume(path):
    return run(["btrfs", "subvolume", "show", path]).returncode == 0


def delete_subvolume(path):
    if is_subvolume(path):
        must(["btrfs", "subvolume", "delete", path])


def producer_run():
    p = run([PRODUCER, "--root", ROOT, "--repo", REPO_PATCH,
             "--restic", PATCH_BIN, "--owner", "BENCH",
             "--state-dir", STATE],
            env={"RESTIC_PASSWORD": PW})
    timings = {}
    for line in p.stdout.splitlines():
        if line.startswith("TIMINGS "):
            timings = json.loads(line[len("TIMINGS "):])
    return p, timings


def stock_backup():
    delete_subvolume(REFSNAP)
    must(["btrfs", "subvolume", "snapshot", "-r", ROOT, REFSNAP])
    run(["sync"])
    t0 = time.time()
    must([STOCK_BIN, "-r", REPO_STOCK, "backup", REFSNAP],
         env={"RESTIC_PASSWORD": PW})
    return time.time() - t0


def du_bytes(path):
    p = must(["btrfs", "filesystem", "du", "--raw", "-s", path])
    # line: Total Exclusive Shared Filename (bytes with --raw)
    for line in p.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[-1] == path:
            return {"total": int(parts[0]), "exclusive": int(parts[1]),
                    "shared": int(parts[2])}
    raise RuntimeError("du parse failed: " + p.stdout)


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


def main():
    os.makedirs(BASE, exist_ok=True)
    results = {}
    if not os.path.exists(ROOT):
        must(["btrfs", "subvolume", "create", ROOT])
        t0 = time.time()
        must(["cp", "-a", "--reflink=always", "/data/corpus/.", ROOT + "/"])
        results["setup_reflink_copy_s"] = round(time.time() - t0, 2)
        print(f"setup: reflink copy {results['setup_reflink_copy_s']}s",
              flush=True)
    for repo, binary in ((REPO_PATCH, PATCH_BIN), (REPO_STOCK, STOCK_BIN)):
        if not os.path.exists(repo):
            must([binary, "-r", repo, "init"], env={"RESTIC_PASSWORD": PW})
    if not os.path.exists(RETAINED):
        t0 = time.time()
        p, timings = producer_run()
        assert p.returncode == 0 and "INIT" in p.stdout, p.stdout + p.stderr
        results["init_total_s"] = round(time.time() - t0, 2)
        print(f"init: producer initial backup {results['init_total_s']}s",
              flush=True)
        t0 = time.time()
        stock_backup()
        results["init_stock_s"] = round(time.time() - t0, 2)
        print(f"init: stock backup {results['init_stock_s']}s", flush=True)

    rng = random.Random(20261026)
    shape_fns = [
        ("spread", lambda files: shapes.apply_spread(ROOT, files, rng)),
        ("concentrated",
         lambda files: shapes.apply_concentrated(ROOT, files, rng)),
        ("full-rewrite", lambda files: apply_full_rewrite(ROOT, files, rng)),
    ]
    for label, fn in shape_fns:
        files = shapes.inventory(ROOT)
        t0 = time.time()
        ops = fn(files)
        mutation_s = time.time() - t0
        run(["sync"])
        p, timings = producer_run()
        ok = p.returncode == 0 and "RESULT patch" in p.stdout
        stock_s = stock_backup()
        pin = du_bytes(RETAINED)
        rec = {
            "ops": len(ops), "mutation_s": round(mutation_s, 2),
            "producer_ok": ok,
            "snapshot_s": round(timings.get("snapshot_s", -1), 2),
            "enumeration_diff_s": round(timings.get("diff_s", -1), 2),
            "patch_backup_s": round(timings.get("backup_s", -1), 2),
            "producer_total_s": round(timings.get("total_s", -1), 2),
            "stock_full_backup_s": round(stock_s, 2),
            "retained_total_bytes": pin["total"],
            "retained_exclusive_bytes": pin["exclusive"],
            "retained_shared_bytes": pin["shared"],
        }
        results[label] = rec
        print(f"=== {label}: {json.dumps(rec)}", flush=True)
        if not ok:
            print(p.stdout[-2000:], p.stderr[-2000:], flush=True)
            sys.exit(1)
    with open(OUT, "w") as f:
        json.dump(results, f, indent=2)
    print("wrote", OUT)


if __name__ == "__main__":
    main()
