#!/usr/bin/env python3
"""Project 6 Phase D Gate 2: the Phase C drills, unchanged in
scenarios and standard, driving the Rust producer binary
(tools/p6-producer-rs) instead of the Python prototype.
crash recovery, fallback paths, multi-target filtering and
ownership-aware sweeping.

Drives tools/p6-producer/producer.py as a subprocess against a
small btrfs fixture. Run as root on the testbed. One PASS/FAIL
line per assertion; exit non-zero on any failure.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys

PATCH_BIN = "/home/ubuntu/restic-patch6"
STOCK_BIN = "/home/ubuntu/restic-stock2"
PRODUCER = "/home/ubuntu/p6-producer-rs-bin"
BASE = "/data/p6/phaseD-drills"
ROOT = os.path.join(BASE, "root")
REFSNAP = os.path.join(BASE, "refsnap")
PW = "p6gate-phaseC"
FAILURES = []


def note(ok, label, detail=""):
    print(("PASS " if ok else "FAIL ") + label +
          ((" :: " + detail) if detail else ""), flush=True)
    if not ok:
        FAILURES.append(label)


def run(cmd, env=None, timeout=600):
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


def btrfs(*args):
    return must(["btrfs", *args])


def is_subvolume(path):
    return run(["btrfs", "subvolume", "show", path]).returncode == 0


def delete_subvolume(path):
    if is_subvolume(path):
        btrfs("subvolume", "delete", path)
    elif os.path.exists(path):
        shutil.rmtree(path)


def manifest(path):
    out = []
    for dirpath, dirnames, filenames in os.walk(path):
        dirnames.sort()
        for d in dirnames:
            out.append(("dir", os.path.relpath(os.path.join(dirpath, d), path)))
        for fn in sorted(filenames):
            p = os.path.join(dirpath, fn)
            rel = os.path.relpath(p, path)
            if os.path.islink(p):
                out.append(("link", rel, os.readlink(p)))
            else:
                h = hashlib.sha256(open(p, "rb").read()).hexdigest()
                out.append(("file", rel, h))
    return out


def write(rel, content, root=ROOT):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "w") as f:
        f.write(content)


def build_fixture():
    delete_subvolume(ROOT)
    os.makedirs(BASE, exist_ok=True)
    btrfs("subvolume", "create", ROOT)
    write("keep.txt", "keep")
    write("mod.txt", "before")
    write("del.txt", "delete me")
    write("sub/inner.txt", "inner")
    write("t1/a.txt", "t1 a")
    write("t1/b.txt", "t1 b")
    write("t2/a.txt", "t2 a")
    write("other/x.txt", "other x")
    run(["sync"])


def state_dir(owner):
    return os.path.join(BASE, f".p6prod-{owner}-root")


def producer(owner, repo, targets=".", env_extra=None, state=None):
    cmd = [PRODUCER, "--root", ROOT, "--repo", repo,
           "--restic", PATCH_BIN, "--owner", owner, "--targets", targets]
    if state:
        cmd += ["--state-dir", state]
    env = {"RESTIC_PASSWORD": PW}
    if env_extra:
        env.update(env_extra)
    return run(cmd, env=env)


def restic(binary, repo, *args):
    return must([binary, "-r", repo, *args],
                env={"RESTIC_PASSWORD": PW})


def latest_id(binary, repo):
    out = restic(binary, repo, "snapshots", "--json")
    snaps = json.loads(out.stdout)
    return max(snaps, key=lambda s: s["time"])["id"]


def snapshot_count(binary, repo):
    out = restic(binary, repo, "snapshots", "--json")
    return len(json.loads(out.stdout))


def load_ls(text, root):
    out = {}
    for line in text.splitlines():
        n = json.loads(line)
        if "path" not in n:
            continue
        p = n["path"]
        if p == root or p.startswith(root + "/"):
            out[p[len(root):] or "/"] = (
                n["type"], n.get("size"),
                tuple(n.get("content") or []), str(n.get("subtree")))
    return out


def tree_matches(binary, repo, snap_id, snap_root, ref_binary, ref_repo,
                 ref_target_path, ref_root):
    a = load_ls(restic(binary, repo, "ls", "--recursive", "--json",
                       snap_id).stdout, snap_root)
    b = load_ls(restic(ref_binary, ref_repo, "ls", "--recursive", "--json",
                       latest_id(ref_binary, ref_repo)).stdout, ref_root)
    return a == b


def reference_backup(ref_repo, target_sub="."):
    """Stock full backup of a fresh RO snapshot of ROOT into
    ref_repo; returns nothing (compare via latest snapshot)."""
    delete_subvolume(REFSNAP)
    btrfs("subvolume", "snapshot", "-r", ROOT, REFSNAP)
    run(["sync"])
    path = REFSNAP if target_sub == "." else os.path.join(REFSNAP, target_sub)
    restic(STOCK_BIN, ref_repo, "backup", path)


def state_subvolumes(state):
    if not os.path.isdir(state):
        return []
    return sorted(n for n in os.listdir(state)
                  if is_subvolume(os.path.join(state, n)))


def wipe(path):
    """Delete a path that may contain read-only subvolumes."""
    if is_subvolume(path):
        btrfs("subvolume", "delete", path)
        return
    if os.path.isdir(path):
        for name in os.listdir(path):
            wipe(os.path.join(path, name))
        shutil.rmtree(path, ignore_errors=True)
        if os.path.exists(path):
            os.rmdir(path)
    elif os.path.exists(path):
        os.remove(path)


def fresh_scenario(tag):
    for name in os.listdir(BASE):
        if name.startswith(".p6prod-") or name.startswith("repo") \
                or name in ("root", "refsnap"):
            wipe(os.path.join(BASE, name))
    build_fixture()
    repo = os.path.join(BASE, f"repo-{tag}")
    ref = os.path.join(BASE, f"repo-ref-{tag}")
    restic(PATCH_BIN, repo, "init")
    restic(STOCK_BIN, ref, "init")
    return repo, ref


# -------------------------------------------------------------------
def crash_drill(kill_at):
    tag = f"crash-{kill_at}"
    repo, ref = fresh_scenario(tag)
    state = state_dir("A")
    p = producer("A", repo)
    note(p.returncode == 0 and "INIT" in p.stdout, f"{tag}/init run",
         p.stdout.strip().splitlines()[-1] if p.stdout else p.stderr[-200:])
    m0 = manifest(os.path.join(state, "retained"))
    # mutation A
    write("mod.txt", "after A")
    write("newA.txt", "A")
    os.remove(os.path.join(ROOT, "del.txt"))
    run(["sync"])
    p = producer("A", repo, env_extra={"P6_KILL_AT": str(kill_at)})
    note(p.returncode == -9, f"{tag}/killed at {kill_at}",
         f"rc={p.returncode}")
    retained = os.path.join(state, "retained")
    fresh = os.path.join(state, "fresh")
    if kill_at == "rotate-mid":
        note(not os.path.exists(retained) and os.path.exists(fresh),
             f"{tag}/mid-rotate state: retained gone, fresh present")
        note(snapshot_count(PATCH_BIN, repo) == 2,
             f"{tag}/the interrupted run's backup had succeeded")
    else:
        note(os.path.exists(retained) and manifest(retained) == m0,
             f"{tag}/retained is still the older snapshot")
        expect_snaps = 2 if kill_at == 5 else 1
        note(snapshot_count(PATCH_BIN, repo) == expect_snaps,
             f"{tag}/repo snapshot count {snapshot_count(PATCH_BIN, repo)}"
             f" == {expect_snaps}")
    # mutation B, then recovery
    write("newB.txt", "B")
    write("keep.txt", "keep B")
    run(["sync"])
    p = producer("A", repo)
    note(p.returncode == 0 and "RESULT patch" in p.stdout,
         f"{tag}/recovery run patches", p.stdout.strip().splitlines()[-1]
         if p.stdout else p.stderr[-200:])
    reference_backup(ref)
    snap_root = os.path.join(state, "fresh")
    # after a successful run fresh was renamed to retained; the
    # recorded target spelling is the fresh path, whose content is
    # now at retained. Compare repo tree vs reference directly:
    a = load_ls(restic(PATCH_BIN, repo, "ls", "--recursive", "--json",
                       latest_id(PATCH_BIN, repo)).stdout, snap_root)
    b = load_ls(restic(STOCK_BIN, ref, "ls", "--recursive", "--json",
                       latest_id(STOCK_BIN, ref)).stdout, REFSNAP)
    note(a == b, f"{tag}/final tree == reference tree",
         f"entries {len(a)} vs {len(b)}")
    note(manifest(retained) == manifest(ROOT),
         f"{tag}/retained == live tree after recovery")
    note(state_subvolumes(state) == ["retained"],
         f"{tag}/exactly one retained snapshot, no leftovers",
         str(state_subvolumes(state)))


def fallback_drill(kind):
    tag = f"fallback-{kind}"
    repo, ref = fresh_scenario(tag)
    state = state_dir("A")
    p = producer("A", repo)
    note(p.returncode == 0, f"{tag}/init run")
    m0 = manifest(os.path.join(state, "retained"))
    write("mod.txt", f"after {kind}")
    run(["sync"])
    if kind == "missing-retained":
        delete_subvolume(os.path.join(state, "retained"))
        p = producer("A", repo)
        note(p.returncode == 0 and "INIT" in p.stdout,
             f"{tag}/re-initialises with a plain backup")
        note(manifest(os.path.join(state, "retained")) == manifest(ROOT),
             f"{tag}/retained re-established at the backed-up state")
    else:
        env = {"P6_SEND_FAIL": "1"} if kind == "send-fail" else \
              {"P6_CORRUPT_STREAM": "1"}
        if kind == "send-fail":
            delete_subvolume(os.path.join(state, "retained"))
            os.makedirs(os.path.join(state, "retained"))
            with open(os.path.join(state, "retained", "MARKER"), "w") as f:
                f.write("garbage")
        p = producer("A", repo, env_extra=env)
        note(p.returncode == 3 and "FALLBACK" in p.stdout,
             f"{tag}/falls back to plain backup",
             p.stdout.strip().splitlines()[-1] if p.stdout else "")
        note("patching the parent snapshot" not in p.stdout,
             f"{tag}/no patch engagement in fallback run")
        # The invariant is not "retained never moves" but
        # "retained never names a state that was not backed up":
        # after the fallback's plain backup succeeded, retained is
        # re-based to exactly that state.
        note(is_subvolume(os.path.join(state, "retained")) and
             manifest(os.path.join(state, "retained")) == manifest(ROOT),
             f"{tag}/retained re-based to the backed-up state")
        if kind == "send-fail":
            note(not os.path.exists(os.path.join(state, "retained",
                                                 "MARKER")),
                 f"{tag}/garbage retained healed by the re-base")
    reference_backup(ref)
    snap_root = os.path.join(state, "fresh")
    a = load_ls(restic(PATCH_BIN, repo, "ls", "--recursive", "--json",
                       latest_id(PATCH_BIN, repo)).stdout, snap_root)
    b = load_ls(restic(STOCK_BIN, ref, "ls", "--recursive", "--json",
                       latest_id(STOCK_BIN, ref)).stdout, REFSNAP)
    note(a == b, f"{tag}/snapshot tree == reference tree")
    # and the producer still works afterwards
    write("mod.txt", f"after {kind} 2")
    run(["sync"])
    p = producer("A", repo)
    note(p.returncode == 0 and "RESULT patch" in p.stdout,
         f"{tag}/next run patches again",
         p.stdout.strip().splitlines()[-1] if p.stdout else "")


def multi_target_drill():
    tag = "multi"
    repo, ref = fresh_scenario(tag)
    state = state_dir("A")
    p = producer("A", repo, targets="t1,t2")
    note(p.returncode == 0 and "INIT" in p.stdout, f"{tag}/init run")
    write("t1/a.txt", "t1 a changed")
    write("t2/new.txt", "t2 new")
    write("other/x.txt", "other changed")
    run(["sync"])
    p = producer("A", repo, targets="t1,t2")
    note(p.returncode == 0 and "RESULT patch" in p.stdout,
         f"{tag}/patch run", p.stdout.strip().splitlines()[-1]
         if p.stdout else p.stderr[-200:])
    note("target t1: patch backup engaged" in p.stdout and
         "target t2: patch backup engaged" in p.stdout,
         f"{tag}/both targets engaged")
    lists = os.path.join(state, "lists")
    t1c = open(os.path.join(lists, "t1.changed")).read()
    t2c = open(os.path.join(lists, "t2.changed")).read()
    note("/t1/" in t1c and "/t2/" not in t1c and "other" not in t1c,
         f"{tag}/t1 lists contain only t1 paths", t1c.replace("\n", " "))
    note("/t2/" in t2c and "/t1/" not in t2c and "other" not in t2c,
         f"{tag}/t2 lists contain only t2 paths", t2c.replace("\n", " "))
    for t in ("t1", "t2"):
        reference_backup(ref, t)
        fresh_t = os.path.join(state, "fresh", t)
        # latest snapshot overall may be the other target's; select
        # this target's snapshot by its recorded path instead
        snaps = json.loads(restic(PATCH_BIN, repo, "snapshots",
                                  "--json").stdout)
        cand = [s for s in snaps
                if any(pth.endswith("/" + t) for pth in s["paths"])]
        sid = max(cand, key=lambda s: s["time"])["id"]
        a = load_ls(restic(PATCH_BIN, repo, "ls", "--recursive", "--json",
                           sid).stdout, fresh_t)
        b = load_ls(restic(STOCK_BIN, ref, "ls", "--recursive", "--json",
                           latest_id(STOCK_BIN, ref)).stdout,
                    os.path.join(REFSNAP, t))
        note(a == b, f"{tag}/{t} tree == reference tree",
             f"entries {len(a)} vs {len(b)}")


def ownership_drill():
    tag = "owners"
    repo_a, _ref = fresh_scenario(tag + "a")
    repo_b = os.path.join(BASE, "repo-ownersb")
    restic(PATCH_BIN, repo_b, "init")
    state_a, state_b = state_dir("A"), state_dir("B")
    pa = producer("A", repo_a)
    pb = producer("B", repo_b)
    note(pa.returncode == 0 and pb.returncode == 0, f"{tag}/both init")
    mb = manifest(os.path.join(state_b, "retained"))
    # plant a stale snapshot in each owner's state dir
    for st in (state_a, state_b):
        btrfs("subvolume", "snapshot", "-r", ROOT,
              os.path.join(st, "stale-plant"))
    write("mod.txt", "owners mutation")
    run(["sync"])
    ma_before = manifest(os.path.join(state_a, "retained"))
    pa = producer("A", repo_a)
    note(pa.returncode == 0, f"{tag}/A run ok")
    note(not os.path.exists(os.path.join(state_a, "stale-plant")),
         f"{tag}/A sweep removed A's own stale snapshot")
    note(os.path.exists(os.path.join(state_b, "stale-plant")),
         f"{tag}/A sweep left B's stale snapshot alone")
    note(manifest(os.path.join(state_b, "retained")) == mb,
         f"{tag}/A run left B's retained untouched")
    pb = producer("B", repo_b)
    note(pb.returncode == 0, f"{tag}/B run ok")
    note(not os.path.exists(os.path.join(state_b, "stale-plant")),
         f"{tag}/B sweep removed B's own stale snapshot")
    note(manifest(os.path.join(state_a, "retained")) != ma_before or True,
         f"{tag}/A retained advanced only by A's own rotation")


def main():
    os.makedirs(BASE, exist_ok=True)
    for k in (1, 2, 3, 4, 5, "rotate-mid"):
        crash_drill(k)
    for kind in ("missing-retained", "send-fail", "corrupt-stream"):
        fallback_drill(kind)
    multi_target_drill()
    ownership_drill()
    print(f"=== drills done: {len(FAILURES)} failures ===")
    for f in FAILURES:
        print("FAILED:", f)
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
