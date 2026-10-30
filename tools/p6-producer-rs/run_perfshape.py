#!/usr/bin/env python3
"""Performance gate for the atime filter: the mass-atime shape
the Phase A corpus never had.

Fixture (built by p6-perfshape-setup.py on the testbed, recorded
in /data/p6/perfshape/expected.json): the full 200k-file corpus
reflink-copied into a subvolume, a retained snapshot, an
atime-only sweep over every entry (atime set, mtime preserved to
the nanosecond), a handful of substantive changes, a fresh
snapshot, and one spooled `btrfs send --no-data` stream.

This runner times `parse` for two producer binaries against the
identical stream and trees (the pre-change build via
P6_RS_BIN_V4, the candidate via P6_RS_BIN) and asserts:

  - the two builds' changed lists, deleted lists and atime_only
    counts are identical (the candidate is a faster source for
    the same facts, not a new rule);
  - the substantive file set is contained in the changed list
    and the expected deletions in the deleted list;
  - the candidate's parse wall time fits the budget derived
    from the send-only wall time S recorded at setup on this
    same shape and machine: parse_s <= S + 120 s. The send is
    the irreducible kernel work of the architecture; a resolver
    within a fixed two-minute allowance of it has removed the
    per-candidate term (v4's parse time is printed for
    contrast and carries no assertion).

Run as the ubuntu user on the testbed; parse runs under sudo,
as in run_noatime.py.
"""
import json
import os
import subprocess
import sys
import time

BASE = "/data/p6/perfshape"
RETAINED = os.path.join(BASE, "retained")
FRESH = os.path.join(BASE, "fresh")
STREAM = os.path.join(BASE, "stream.bin")
BIN_V4 = os.environ.get("P6_RS_BIN_V4", "/home/ubuntu/p6-producer-rs-bin-v4")
BIN_V5 = os.environ.get("P6_RS_BIN", "/home/ubuntu/p6-producer-rs-bin-v5")


def parse_with(binary, tag):
    changed_f = os.path.join(BASE, f"{tag}.changed")
    deleted_f = os.path.join(BASE, f"{tag}.deleted")
    t0 = time.time()
    p = subprocess.run(["sudo", binary, "parse", "--stream", STREAM,
                        "--retained", RETAINED, "--fresh", FRESH,
                        "--changed-out", changed_f,
                        "--deleted-out", deleted_f],
                       capture_output=True, text=True)
    wall = time.time() - t0
    if p.returncode != 0:
        raise RuntimeError(f"{tag} parse failed rc={p.returncode}: "
                           f"{p.stderr.strip()}")
    summary = json.loads(p.stdout.strip().splitlines()[-1])
    return {
        "tag": tag,
        "wall_s": round(wall, 2),
        "changed": sorted(open(changed_f, "rb").read().splitlines()),
        "deleted": sorted(open(deleted_f, "rb").read().splitlines()),
        "atime_only": summary.get("atime_only", -1),
        "events": summary.get("events", []),
    }


def main():
    with open(os.path.join(BASE, "expected.json")) as fh:
        meta = json.load(fh)
    send_s = meta["send_s"]
    budget = send_s + 120.0
    print(f"shape: {meta['corpus_files']} files, "
          f"{meta['swept_entries']} swept entries, "
          f"stream {meta['stream_bytes']} bytes, "
          f"send-only {send_s:.2f}s, budget parse <= {budget:.2f}s",
          flush=True)

    res = {}
    for tag, binary in (("v4", BIN_V4), ("v5", BIN_V5)):
        res[tag] = parse_with(binary, tag)
        print(f"[{tag}] parse {res[tag]['wall_s']:.2f}s "
              f"changed={len(res[tag]['changed'])} "
              f"deleted={len(res[tag]['deleted'])} "
              f"atime_only={res[tag]['atime_only']} "
              f"events={res[tag]['events']}", flush=True)

    failures = []
    if res["v5"]["changed"] != res["v4"]["changed"]:
        failures.append("changed lists differ between v5 and v4")
    if res["v5"]["deleted"] != res["v4"]["deleted"]:
        failures.append("deleted lists differ between v5 and v4")
    if res["v5"]["atime_only"] != res["v4"]["atime_only"]:
        failures.append("atime_only counts differ between v5 and v4")
    changed = set(x.decode("utf-8", "surrogateescape")
                  for x in res["v5"]["changed"])
    deleted = set(x.decode("utf-8", "surrogateescape")
                  for x in res["v5"]["deleted"])
    for f in meta["expected_changed_files"]:
        if f not in changed:
            failures.append(f"substantive change missing: {f}")
    for f in meta["expected_deleted"]:
        if f not in deleted:
            failures.append(f"expected deletion missing: {f}")
    if res["v5"]["wall_s"] > budget:
        failures.append(f"v5 parse {res['v5']['wall_s']:.2f}s "
                        f"exceeds budget {budget:.2f}s")
    print(f"speedup vs v4 parse: "
          f"{res['v4']['wall_s'] / max(res['v5']['wall_s'], 0.01):.1f}x",
          flush=True)
    if failures:
        for f in failures:
            print("FAIL " + f, flush=True)
        return 1
    print("PASS perfshape", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
