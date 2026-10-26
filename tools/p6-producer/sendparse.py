#!/usr/bin/env python3
"""Producer for Project 6: derive changed/deleted path lists from a
`btrfs send --no-data` stream between two read-only snapshots, parsed
from the text rendering produced by `btrfs receive --dump`.

Empirical basis (btrfs-progs 6.6.3, OCI testbed, 2026-10-26): the
dump renders one command per line, paths prefixed with ./<snapname>/,
e.g.:

    snapshot        ./snap1   uuid=... transid=... parent_uuid=...
    update_extent   ./snap1/dirA/f1.txt   offset=0 len=15
    utimes          ./snap1/dirA/f1.txt   atime=... mtime=... ctime=...
    link            ./snap1/dirA/f2renamed.txt   dest=f2.txt
    unlink          ./snap1/f2.txt
    mkfile          ./snap1/o263-205-0
    rename          ./snap1/o263-205-0   dest=./snap1/newfile.txt
    symlink         ./snap1/o265-205-0   dest=newfile.txt
    set_xattr       ./snap1/newfile.txt   name=user.other data=zzz len=3

Semantics that fall out of the format and drive the parser:

  * A rename of a surviving inode is rendered as link + unlink
    (new name linked to the inode, old name unlinked), NOT as a
    rename command. The rename command is used for the kernel's
    staging names (o<ino>-<transid>-0) under which newly created
    inodes first appear, and possibly for directory moves.
  * dest= means three different things: for link, the existing tree
    path the inode is linked from (no ./ prefix); for rename, the
    new tree path (with ./ prefix); for symlink, the link target
    (arbitrary string, not a tree path at all).
  * Content changes appear as update_extent (stream v2) / write /
    clone / truncate / fallocate commands, never as data in
    --no-data mode.

List contract produced here (consumed by the Phase B patch mode):

  changed: paths present in the fresh tree that must be archived
    fresh (created, content- or metadata-modified, rename/link
    targets). Rename = changed at the new path.
  deleted: paths present in the retained tree and absent from the
    fresh tree. A path that was unlinked and recreated in the same
    interval is changed, not deleted (the patch replaces its node).

Resolution rules applied after collecting raw operations:

  * Staging-name aliases are substituted to their final paths.
  * Operations on a multiply-linked file are expanded to every fresh
    path of that inode (the kernel names one representative path;
    the change applies to the inode, hence to all its names).
  * An operation naming a path that is not in the fresh tree is
    resolved through the inode number to the inode's fresh paths.
  * Final sets are intersected with tree reality: changed only
    contains fresh paths, deleted only retained-but-not-fresh paths
    that were not recreated.

Known prototype limitation (the reason the brief reserves a TLV
parser for production): the text format separates a path from its
key=value arguments by whitespace, so a path containing a substring
like " mode=" would be ambiguous. Fixture names avoid argument-like
substrings; names with ordinary spaces and non-ASCII characters are
handled and tested. Ambiguous dest= splits are validated against the
two tree walks and reported as events when not uniquely resolvable.
"""

import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

TEMP_RE = re.compile(r"^o\d+-\d+-\d+$")
CREATE_CMDS = {"mkfile", "mkdir", "mknod", "mkfifo", "mksock", "symlink"}
CONTENT_CMDS = {"update_extent", "write", "clone", "fallocate", "truncate"}
META_CMDS = {"chmod", "chown", "utimes", "set_xattr", "remove_xattr"}
DELETE_CMDS = {"unlink", "rmdir"}
KNOWN_CMDS = CREATE_CMDS | CONTENT_CMDS | META_CMDS | DELETE_CMDS | {
    "link", "rename", "snapshot", "subvol"}

# Argument tails for commands whose path is followed by key=value
# arguments (all argument values are whitespace-free in the dump,
# except set_xattr data, handled separately).
_ARG_TAIL = {
    "chmod": re.compile(r"\s+mode=\S+$"),
    "chown": re.compile(r"\s+gid=\S+\s+uid=\S+$"),
    "utimes": re.compile(r"\s+atime=\S+\s+mtime=\S+\s+ctime=\S+$"),
    "update_extent": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    "write": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    "truncate": re.compile(r"\s+size=\S+$"),
    "remove_xattr": re.compile(r"\s+name=\S+$"),
    "fallocate": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    "clone": re.compile(r"\s+offset=\S+\s+len=\S+(\s+src=\S+)?$"),
}


def _is_temp(path):
    return bool(TEMP_RE.match(path.rsplit("/", 1)[-1]))


class DumpParser:
    def __init__(self, snap_name, old_records, new_records):
        self.prefix = "./" + snap_name + "/"
        self.old = old_records
        self.new = new_records
        self.raw_changed = set()
        self.raw_deleted = set()
        self.alias = {}          # staging name -> final path
        self.events = []         # findings worth reporting
        self.new_ino = {}
        for p, r in new_records.items():
            if r["type"] == "file":
                self.new_ino.setdefault(r["ino"], []).append(p)
        self.old_ino = {}
        for p, r in old_records.items():
            if r["type"] == "file":
                self.old_ino.setdefault(r["ino"], []).append(p)

    # -- path helpers ------------------------------------------------
    def _strip(self, raw):
        """Strip the ./<snap>/ prefix; the snapshot root becomes ""."""
        if raw == "./" + self.prefix.strip("./").rstrip("/"):
            return ""
        if raw.startswith(self.prefix):
            return raw[len(self.prefix):]
        if raw.endswith("/") and raw[:-1].endswith(self.prefix.rstrip("/")):
            return ""
        return raw  # already tree-relative (link dest form)

    def _split_dest(self, rest, cmd):
        """Split '<path> dest=<value>' trying every ' dest=' split.

        Returns (path, value) with the split validated per command:
        link: value must be a retained/fresh/staging path;
        rename: value (stripped) must be a fresh or staging path;
        symlink: value is a target string, unvalidated, so the first
        split after a staging-name path wins, else the last split.
        """
        splits = []
        start = 0
        while True:
            i = rest.find(" dest=", start)
            if i < 0:
                break
            splits.append((rest[:i], rest[i + 6:]))
            start = i + 1
        if not splits:
            self.events.append({"kind": "dest-missing", "cmd": cmd,
                                "line": rest})
            return rest, ""
        valid = []
        for path, value in splits:
            sp = self._strip(path)
            if cmd == "link":
                sv = self._strip(value)
                if sv in self.old or sv in self.new or _is_temp(sv):
                    valid.append((sp, sv))
            elif cmd == "rename":
                sv = self._strip(value)
                if sv in self.new or _is_temp(sv) or sv in self.alias:
                    valid.append((sp, sv))
            elif cmd == "symlink":
                if _is_temp(sp):
                    valid.append((sp, value))
        if len(valid) == 1:
            return valid[0]
        if cmd == "symlink" and splits:
            # Symlink on a real path: the target is the remainder
            # after the last plausible split; prefer the longest path
            # that exists in the fresh tree.
            for path, value in reversed(splits):
                sp = self._strip(path)
                if sp in self.new:
                    return sp, value
            p, v = splits[-1]
            return self._strip(p), v
        if valid:
            self.events.append({"kind": "dest-ambiguous", "cmd": cmd,
                                "line": rest, "candidates": valid})
            return valid[0]
        p, v = splits[0]
        self.events.append({"kind": "dest-unvalidated", "cmd": cmd,
                            "line": rest})
        return self._strip(p), self._strip(v) if cmd != "symlink" else v

    def _extract_path(self, cmd, rest):
        if cmd == "set_xattr":
            m = re.match(r"^(.*)\s+name=(\S+)\s+data=(.*)\s+len=(\d+)$",
                         rest)
            if m and len(m.group(3)) == int(m.group(4)):
                return m.group(1)
            self.events.append({"kind": "setxattr-unparsed", "line": rest})
            return rest
        tail = _ARG_TAIL.get(cmd)
        if tail is not None:
            m = tail.search(rest)
            if m:
                return rest[:m.start()]
            self.events.append({"kind": "tail-unmatched", "cmd": cmd,
                                "line": rest})
            return rest
        return rest

    # -- main entry --------------------------------------------------
    def parse(self, dump_text):
        for line in dump_text.splitlines():
            line = line.rstrip()
            if not line or line.startswith("At subvol"):
                continue
            parts = line.split(None, 1)
            cmd = parts[0]
            rest = parts[1].strip() if len(parts) > 1 else ""
            if cmd in ("snapshot", "subvol"):
                continue
            if cmd not in KNOWN_CMDS:
                self.events.append({"kind": "unknown-command",
                                    "cmd": cmd, "line": line})
                continue
            if cmd in ("link", "rename", "symlink"):
                path, value = self._split_dest(rest, cmd)
                if cmd == "link":
                    self.raw_changed.add(path)
                elif cmd == "rename":
                    self.raw_deleted.add(path)
                    self.raw_changed.add(value)
                    if _is_temp(path):
                        self.alias[path] = value
                elif cmd == "symlink":
                    self.raw_changed.add(path)
                continue
            path = self._strip(self._extract_path(cmd, rest))
            if cmd in DELETE_CMDS:
                self.raw_deleted.add(path)
            else:
                self.raw_changed.add(path)
        return self._resolve()

    def _resolve(self):
        def subst(p):
            seen = set()
            while p in self.alias and p not in seen:
                seen.add(p)
                p = self.alias[p]
            return p

        changed = set()
        for p in self.raw_changed:
            changed.add(subst(p))
        deleted_cand = set()
        for p in self.raw_deleted:
            deleted_cand.add(subst(p))

        # Expand inode-level operations to all fresh paths of the
        # inode, resolving op paths that name a retained location of
        # a surviving inode.
        expanded = set()
        for p in changed:
            if p in self.new:
                expanded.add(p)
                ino = self.new[p].get("ino")
                if self.new[p]["type"] == "file":
                    expanded.update(self.new_ino.get(ino, []))
            elif p in self.old:
                ino = self.old[p].get("ino")
                sibs = self.new_ino.get(ino, [])
                if sibs:
                    expanded.update(sibs)
                else:
                    self.events.append({"kind": "op-path-vanished",
                                        "path": p})
            elif not _is_temp(p):
                self.events.append({"kind": "op-path-unresolved",
                                    "path": p})
        changed = {p for p in expanded if p in self.new}

        deleted = set()
        for p in deleted_cand:
            if _is_temp(p):
                continue
            if p in self.old and p not in self.new and p not in changed:
                deleted.add(p)
        return changed, deleted, self.events


def parse_dump(dump_text, snap_name, old_records, new_records):
    return DumpParser(snap_name, old_records, new_records).parse(dump_text)


# ---------------------------------------------------------------------
# Producer: snapshot + send + dump cycle
# ---------------------------------------------------------------------

def _run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True,
                          text=True, **kw)


def snapshot_fresh(root, fresh):
    """(Re)create the read-only fresh snapshot of root at fresh."""
    if os.path.exists(fresh):
        _run(["sudo", "btrfs", "subvolume", "delete", fresh])
    _run(["sudo", "btrfs", "subvolume", "snapshot", "-r", root, fresh])
    _run(["sync"])


def produce(root, retained, fresh, workdir):
    """Run one producer cycle. retained must already exist (a
    read-only snapshot); a fresh snapshot of root is taken, and the
    send stream between them is dumped to text.

    Returns (dump_text, timings dict). Paths in the dump are prefixed
    with ./<basename(fresh)>/.
    """
    os.makedirs(workdir, exist_ok=True)
    stream = os.path.join(workdir, "stream.bin")
    dump = os.path.join(workdir, "dump.txt")
    t0 = time.time()
    snapshot_fresh(root, fresh)
    t1 = time.time()
    with open(stream, "wb") as f:
        subprocess.run(["sudo", "btrfs", "send", "--no-data",
                        "-p", retained, fresh],
                       check=True, stdout=f,
                       stderr=subprocess.PIPE)
    t2 = time.time()
    with open(dump, "w") as f:
        subprocess.run(["btrfs", "receive", "--dump", "-f", stream],
                       check=True, stdout=f, stderr=subprocess.PIPE)
    t3 = time.time()
    with open(dump) as f:
        text = f.read()
    return text, {"snapshot_s": t1 - t0, "send_s": t2 - t1,
                  "dump_s": t3 - t2, "stream_bytes": os.path.getsize(stream)}
