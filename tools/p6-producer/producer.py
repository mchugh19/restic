#!/usr/bin/env python3
"""Project 6 producer: the script-side lifecycle that drives
restic's parent-tree patch mode from `btrfs send --no-data` diffs.

One producer owns one root (a btrfs subvolume) and one restic
repository. Its state lives in a state directory it owns alone
(default: a sibling of the root named .p6prod-<owner>-<rootname>):

    retained   read-only snapshot of the root as of the last
               successful patch backup (the diff base)
    fresh      read-only snapshot of the root taken by the
               current run (the diff target and the read source
               for the backup)
    lists/     the list files of the most recent run

Run sequence (numbered; the crash drills kill the producer at
each step):

    1. sweep: delete stale subvolumes inside the own state
       directory (anything that is not `retained` or `fresh`).
       If `retained` is missing but `fresh` exists, a previous
       run was killed mid-rotation after its backups had
       succeeded: adopt `fresh` as `retained` (it is exactly the
       last backed-up state). The sweep never looks outside the
       own state directory, so two producers on the same root
       cannot delete each other's snapshots.
    2. snapshot: fresh := read-only snapshot of the root, sync.
    3. diff: `btrfs send --no-data` retained -> fresh, parsed to
       changed/deleted path lists (sendparse.py).
    4. backup: for each target (a subpath of the root; "." is the
       root itself), filter the lists to the target and run a
       patch-mode backup reading from the fresh snapshot. The
       backup must engage (the engagement line must appear); a
       decline means the lists did not reconcile and counts as a
       producer failure.
    5. rotate: only after every target's patch backup succeeded:
       delete the old retained, rename fresh -> retained.

Failure handling: any failure in steps 3-4 (send error,
unparseable stream, restic error, patch decline) falls back to a
plain full backup of every target from the fresh snapshot. The
retained snapshot is never advanced past a backed-up state:
after a fallback run's plain backups succeed, retained is
re-based to the fresh snapshot (exactly the state just backed
up), which also heals a corrupt retained. With no retained
snapshot at all the run initialises instead: plain backup from
fresh, then retained := fresh. Exit codes: 0 patch or init
success, 3 fallback success, 1 hard failure (even the plain
backup failed).

Crash consistency: a run killed after its patch backups
succeeded but before rotation would otherwise leave the diff
base (retained) one state behind the repository's parent
snapshot, and every later run would decline (its lists name
paths the parent no longer has) and fall back forever. The
producer therefore records a phase marker in state.json:
"backed-up" (with the fresh snapshot's UUID) after step 4, and
"idle" after rotation. At step 1, a "backed-up" marker whose
fresh snapshot is still present is adopted: fresh becomes the
retained snapshot before anything else happens, restoring
diff base == repository parent.

Drill hooks (environment): P6_KILL_AT=<step>|rotate-mid kills
the producer with SIGKILL at that point; P6_CORRUPT_STREAM=1
appends garbage to the send stream before it is parsed;
P6_SEND_FAIL=1 makes the send use a nonexistent parent.
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sendparse  # noqa: E402
import treediff  # noqa: E402


def log(msg):
    print(msg, flush=True)


def run(cmd, env=None, timeout=3600):
    e = dict(os.environ)
    if env:
        e.update(env)
    p = subprocess.run(cmd, capture_output=True, text=True, env=e,
                       timeout=timeout)
    return p


def maybe_kill(point):
    if os.environ.get("P6_KILL_AT") == str(point):
        log(f"KILL HOOK at {point}")
        os.kill(os.getpid(), signal.SIGKILL)


class Producer:
    def __init__(self, args):
        self.root = os.path.abspath(args.root)
        self.repo = args.repo
        self.restic = args.restic
        self.owner = args.owner
        self.targets = args.targets.split(",")
        self.features = args.features
        if args.state_dir:
            self.state = os.path.abspath(args.state_dir)
        else:
            parent = os.path.dirname(self.root)
            base = os.path.basename(self.root)
            self.state = os.path.join(parent, f".p6prod-{self.owner}-{base}")
        self.retained = os.path.join(self.state, "retained")
        self.fresh = os.path.join(self.state, "fresh")
        self.lists_dir = os.path.join(self.state, "lists")
        self.timings = {}

    # -- btrfs helpers -------------------------------------------------
    def is_subvolume(self, path):
        return os.path.isdir(path) and run(
            ["btrfs", "subvolume", "show", path]).returncode == 0

    def delete_subvolume(self, path):
        if self.is_subvolume(path):
            p = run(["btrfs", "subvolume", "delete", path])
            if p.returncode != 0:
                raise RuntimeError(f"delete {path}: {p.stderr}")
        elif os.path.exists(path):
            raise RuntimeError(f"{path} exists and is not a subvolume")

    def snapshot_ro(self, src, dst):
        self.delete_subvolume(dst)
        p = run(["btrfs", "subvolume", "snapshot", "-r", src, dst])
        if p.returncode != 0:
            raise RuntimeError(f"snapshot {src} -> {dst}: {p.stderr}")

    # -- restic --------------------------------------------------------
    def restic_backup(self, target_path, changed=None, deleted=None):
        cmd = [self.restic, "-r", self.repo, "backup", target_path]
        env = {}
        if changed is not None:
            cmd += ["--patch-changed-from", changed,
                    "--patch-deleted-from", deleted]
            env["RESTIC_FEATURES"] = self.features
        p = run(cmd, env=env)
        return p

    # -- phase marker ----------------------------------------------------
    def phase_file(self):
        return os.path.join(self.state, "state.json")

    def read_phase(self):
        try:
            with open(self.phase_file()) as f:
                return json.load(f)
        except Exception:
            return {}

    def write_phase(self, phase, snap_path=None):
        data = {"phase": phase}
        if snap_path is not None:
            data["uuid"] = self.subvolume_uuid(snap_path)
        with open(self.phase_file(), "w") as f:
            json.dump(data, f)

    def subvolume_uuid(self, path):
        p = run(["btrfs", "subvolume", "show", path])
        for line in p.stdout.splitlines():
            line = line.strip()
            if line.startswith("UUID:"):
                return line.split(":", 1)[1].strip()
        return None

    # -- steps ----------------------------------------------------------
    def step1_sweep(self):
        maybe_kill(1)
        log("STEP 1: sweep")
        os.makedirs(self.state, exist_ok=True)
        os.makedirs(self.lists_dir, exist_ok=True)
        marker = self.read_phase()
        if marker.get("phase") == "backed-up":
            if os.path.exists(self.fresh) and \
                    self.subvolume_uuid(self.fresh) == marker.get("uuid"):
                # The previous run's backups succeeded but it was
                # stopped before rotating: adopt its fresh snapshot
                # as the diff base so base == repository parent.
                self.delete_subvolume(self.retained)
                os.rename(self.fresh, self.retained)
                log("STEP 1: adopted backed-up fresh as retained "
                    "(previous run stopped before rotation)")
            else:
                log("STEP 1: backed-up marker without its fresh "
                    "snapshot; clearing the marker, retained kept")
            self.write_phase("idle", self.retained)
        for name in sorted(os.listdir(self.state)):
            if name in ("retained", "fresh", "lists", "state.json",
                        "stream.bin", "dump.txt"):
                continue
            path = os.path.join(self.state, name)
            if self.is_subvolume(path):
                self.delete_subvolume(path)
                log(f"STEP 1: removed own stale snapshot {name}")

    def step2_snapshot(self):
        maybe_kill(2)
        log("STEP 2: fresh snapshot")
        t0 = time.time()
        self.snapshot_ro(self.root, self.fresh)
        run(["sync"])
        self.timings["snapshot_s"] = time.time() - t0

    def step3_diff(self):
        maybe_kill(3)
        log("STEP 3: send diff")
        t0 = time.time()
        stream = os.path.join(self.state, "stream.bin")
        dump = os.path.join(self.state, "dump.txt")
        parent = self.retained
        if os.environ.get("P6_SEND_FAIL"):
            parent = os.path.join(self.state, "nonexistent-parent")
        with open(stream, "wb") as f:
            p = subprocess.run(["btrfs", "send", "--no-data", "-p",
                                parent, self.fresh],
                               stdout=f, stderr=subprocess.PIPE)
        if p.returncode != 0:
            raise RuntimeError(f"btrfs send failed: "
                               f"{p.stderr.decode(errors='replace')}")
        if os.environ.get("P6_CORRUPT_STREAM"):
            with open(stream, "r+b") as f:
                data = bytearray(f.read())
                mid = len(data) // 2
                for i in range(mid, min(mid + 512, len(data))):
                    data[i] ^= 0xFF
                f.seek(0)
                f.write(data)
        with open(dump, "wb") as f:
            p = subprocess.run(["btrfs", "receive", "--dump", "-f", stream],
                               stdout=f, stderr=subprocess.PIPE)
        if p.returncode != 0:
            raise RuntimeError("btrfs receive --dump failed on the stream")
        with open(dump, encoding="utf-8", errors="surrogateescape") as f:
            text = f.read()
        old = treediff.walk(self.retained, hash_content=False)
        new = treediff.walk(self.fresh, hash_content=False)
        changed, deleted, _events = sendparse.parse_dump(
            text, os.path.basename(self.fresh), old, new)
        self.timings["diff_s"] = time.time() - t0
        log(f"STEP 3: {len(changed)} changed, {len(deleted)} deleted")
        return changed, deleted

    def target_path(self, target):
        if target == ".":
            return self.fresh
        return os.path.join(self.fresh, target)

    def filter_lists(self, target, changed, deleted):
        def keep(rel):
            if target == ".":
                return True
            return rel == target or rel.startswith(target + "/")

        def absolute(rel):
            return os.path.join(self.fresh, rel)

        return ([absolute(r) for r in changed if keep(r)],
                [absolute(r) for r in deleted if keep(r)])

    def write_lists(self, target, changed_abs, deleted_abs):
        name = "root" if target == "." else target.replace("/", "_")
        cf = os.path.join(self.lists_dir, name + ".changed")
        df = os.path.join(self.lists_dir, name + ".deleted")
        with open(cf, "w") as f:
            for p in changed_abs:
                f.write(p + "\n")
        with open(df, "w") as f:
            for p in deleted_abs:
                f.write(p + "\n")
        return cf, df

    def step4_patch_backups(self, changed, deleted):
        maybe_kill(4)
        log("STEP 4: patch backups")
        t0 = time.time()
        for target in self.targets:
            c_abs, d_abs = self.filter_lists(target, changed, deleted)
            cf, df = self.write_lists(target, c_abs, d_abs)
            p = self.restic_backup(self.target_path(target), cf, df)
            out = p.stdout + p.stderr
            if p.returncode != 0:
                raise RuntimeError(f"patch backup of {target} failed: {out}")
            if "patching the parent snapshot" not in out:
                raise RuntimeError(
                    f"patch backup of {target} declined to a full walk: "
                    f"{[l for l in out.splitlines() if 'patch mode' in l]}")
            log(f"STEP 4: target {target}: patch backup engaged "
                f"(changed={len(c_abs)} deleted={len(d_abs)})")
        self.timings["backup_s"] = time.time() - t0
        self.write_phase("backed-up", self.fresh)

    def plain_backups(self, why):
        log(f"plain backups ({why})")
        for target in self.targets:
            p = self.restic_backup(self.target_path(target))
            if p.returncode != 0:
                raise RuntimeError(
                    f"plain backup of {target} failed: {p.stdout}{p.stderr}")
            log(f"plain backup of target {target} done")

    def step5_rotate(self):
        maybe_kill(5)
        log("STEP 5: rotate retained")
        self.delete_subvolume(self.retained)
        maybe_kill("rotate-mid")
        os.rename(self.fresh, self.retained)
        self.write_phase("idle", self.retained)

    def remove_any(self, path):
        if self.is_subvolume(path):
            self.delete_subvolume(path)
        elif os.path.isdir(path):
            shutil.rmtree(path)
        elif os.path.exists(path):
            os.remove(path)

    def rebase_retained(self):
        """Fallback path: retained := fresh, the state whose plain
        backup just succeeded. Also heals a corrupt retained."""
        self.remove_any(self.retained)
        os.rename(self.fresh, self.retained)
        self.write_phase("idle", self.retained)
        log("retained re-based to the just-backed-up state")

    def run(self):
        t_start = time.time()
        os.makedirs(self.state, exist_ok=True)
        self.step1_sweep()
        self.step2_snapshot()
        init = not os.path.exists(self.retained)
        if init:
            log("INIT: no retained snapshot; plain backup, then "
                "retained := fresh")
            try:
                self.plain_backups("initialisation")
            except Exception as e:
                log(f"HARD FAILURE: {e}")
                return 1
            os.rename(self.fresh, self.retained)
            self.write_phase("idle", self.retained)
            log("INIT: retained established")
            self.timings["total_s"] = time.time() - t_start
            log("TIMINGS " + json.dumps(self.timings))
            return 0
        try:
            changed, deleted = self.step3_diff()
            self.step4_patch_backups(changed, deleted)
        except Exception as e:
            log(f"FALLBACK: {e}")
            try:
                self.plain_backups("producer failure")
            except Exception as e2:
                log(f"HARD FAILURE: {e2}")
                return 1
            self.rebase_retained()
            self.timings["total_s"] = time.time() - t_start
            log("TIMINGS " + json.dumps(self.timings))
            log("RESULT fallback (retained re-based, not rotated "
                "past the backup)")
            return 3
        self.step5_rotate()
        self.timings["total_s"] = time.time() - t_start
        log("TIMINGS " + json.dumps(self.timings))
        log("RESULT patch")
        return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--restic", required=True)
    ap.add_argument("--owner", required=True)
    ap.add_argument("--targets", default=".")
    ap.add_argument("--state-dir")
    ap.add_argument("--features", default="patch-mode")
    args = ap.parse_args()
    sys.exit(Producer(args).run())


if __name__ == "__main__":
    main()
