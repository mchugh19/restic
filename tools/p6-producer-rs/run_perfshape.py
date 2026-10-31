#!/usr/bin/env python3
"""Perf-shape measurement (runs ON the OCI testbed).

Measures, on the fixture build_perfshape.py produced in
/data/p6/perfshape (the read-driven mass-atime shape):

  send-only   `btrfs send --no-data` of an identical tree pair,
              the floor no parse can beat in production prepare;
  v4 parse    the reference v4 binary (stat proof per candidate);
  v5 parse    the staged v5 binary (tree-search map), when the
              harness finds it;
  v6 parse    the v6 binary (ctime proof first).

Each parse runs against the spooled stream file, so send
generation is excluded from the parse numbers (byte-identical
input for all three). Reports the wall times and the v6
ctime-proof aggregate, and asserts: v6's changed/deleted lists
and atime_only are byte-identical to v4's, and the changed set
equals the builder's expected substantive set. Budget line: v6
parse <= send-only + 120s (the performance gate's shape; a
design target, not an alarm). With the ctime proof, nearly all
swept candidates drop with no retained lookup at all, so v6 is
expected to land close to the stream-decode floor, far under
both v4 (one lstat per candidate) and v5 (the full tree walk).

Testbed caveat (from the v5 post-mortem): this machine's storage
is SSD; the numbers are functional and relative only. They do
not, and cannot, show production HDD metadata costs.
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
META = os.path.join(BASE, "expected.json")

BINS = {
    "v4": "/home/ubuntu/p6-producer-v4ref",
    "v5": "/home/ubuntu/p6-producer-v5",
    "v6": "/home/ubuntu/p6-producer-v6",
}


def run(cmd, **kw):
    print("+ " + " ".join(cmd), flush=True)
    if "stdout" not in kw and "stderr" not in kw:
        kw["capture_output"] = True
    return subprocess.run(cmd, check=True, text=True, **kw)


def read_list(path):
    with open(path, "rb") as fh:
        data = fh.read()
    if not data:
        return []
    return [x.decode("utf-8", "surrogateescape")
            for x in data.split(b"\n") if x]


def parse(binpath, tag):
    ch_f = f"/tmp/perfshape-{tag}.changed"
    dl_f = f"/tmp/perfshape-{tag}.deleted"
    t0 = time.time()
    r = run(["sudo", binpath, "parse", "--stream", STREAM,
             "--retained", RETAINED, "--fresh", FRESH,
             "--changed-out", ch_f, "--deleted-out", dl_f])
    dt = time.time() - t0
    summary = json.loads(r.stdout.strip().splitlines()[-1])
    summary["_changed_list"] = read_list(ch_f)
    summary["_deleted_list"] = read_list(dl_f)
    return dt, summary


def lists(summary):
    # The list files carry directories unmarked (no trailing
    # slash in this branch's format); the builder's expected
    # set is files only, so directories are filtered against
    # the fresh tree itself.
    ch = sorted(p for p in summary["_changed_list"]
                if not os.path.isdir(os.path.join(FRESH, p)))
    dl = sorted(summary["_deleted_list"])
    return ch, dl


def main():
    meta = json.load(open(META))
    out = {"fixture": meta}

    # send-only floor: send between the retained snapshot and an
    # identical second snapshot of it (zero diff content).
    twin = os.path.join(BASE, "twin")
    run(["sudo", "btrfs", "subvolume", "snapshot", "-r",
         RETAINED, twin])
    try:
        t0 = time.time()
        run(["sudo", "btrfs", "send", "--no-data", "-p", RETAINED,
             twin], stdout=subprocess.DEVNULL)
        out["send_only_s"] = round(time.time() - t0, 2)
    finally:
        run(["sudo", "btrfs", "subvolume", "delete", twin])

    summaries = {}
    for name in ("v4", "v5", "v6"):
        if not os.path.exists(BINS[name]):
            out[f"{name}_parse"] = "binary not present, skipped"
            print(f"{name}: binary not present, skipped", flush=True)
            continue
        dt, s = parse(BINS[name], name)
        summaries[name] = s
        ev = {}
        for e in s["events"]:
            ev.setdefault(e["kind"], []).append(e.get("path"))
        out[f"{name}_parse"] = {
            "wall_s": round(dt, 2),
            "changed": s["changed"],
            "deleted": s["deleted"],
            "atime_only": s["atime_only"],
            "events": ev,
        }
        print(f"{name}: wall {dt:.2f}s "
              f"changed {s['changed']} "
              f"deleted {s['deleted']} "
              f"atime_only {s['atime_only']} events {ev}",
              flush=True)

    assert "v4" in summaries and "v6" in summaries, \
        "v4 and v6 binaries are both required for the comparison"
    ch4, dl4 = lists(summaries["v4"])
    ch6, dl6 = lists(summaries["v6"])
    assert ch6 == ch4, "v6 changed list differs from v4"
    assert dl6 == dl4, "v6 deleted list differs from v4"
    assert summaries["v6"]["atime_only"] == summaries["v4"]["atime_only"], \
        "v6 atime_only differs from v4"
    exp = sorted(meta["expected_changed_files"])
    assert ch6 == exp, (
        f"changed set differs from the expected substantive set: "
        f"missing {sorted(set(exp) - set(ch6))[:10]} "
        f"extra {sorted(set(ch6) - set(exp))[:10]}")
    budget = out["send_only_s"] + 120.0
    out["budget_s"] = budget
    out["v6_within_budget"] = out["v6_parse"]["wall_s"] <= budget
    cp = [e.get("path") for e in summaries["v6"]["events"]
          if e["kind"] == "ctime-proof"]
    out["v6_ctime_proven"] = int(cp[0]) if cp and cp[0] else 0
    out["v6_atime_only"] = summaries["v6"]["atime_only"]
    print(json.dumps(out, indent=1), flush=True)
    print("PERFSHAPE OK", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
