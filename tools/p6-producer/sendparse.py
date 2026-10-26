#!/usr/bin/env python3
"""Producer for Project 6: derive changed/deleted path lists from a
`btrfs send --no-data` stream between two read-only snapshots, parsed
from the text rendering produced by `btrfs receive --dump`.

Empirical basis (btrfs-progs 6.6.3, OCI testbed, 2026-10-26). The
dump renders one command per line, paths prefixed ./<snapname>/ and
padded to a fixed column width, special characters backslash-escaped
(a space in a name renders as "\\ "), e.g.:

    snapshot        ./snap1   uuid=... transid=... parent_uuid=...
    update_extent   ./snap1/dirA/f1.txt   offset=0 len=15
    link            ./snap1/dirA/f2renamed.txt   dest=f2.txt
    unlink          ./snap1/f2.txt
    mkfile          ./snap1/o263-205-0
    rename          ./snap1/o263-205-0   dest=./snap1/newfile.txt
    rename          ./snap1/bulk/d03   dest=./fresh/bulk/rd-d03
    rename          ./snap1/bulk/d22   dest=./snap1/o287-282-0
    unlink          ./snap1/o287-282-0/f00-echo.bin
    rmdir           ./snap1/o287-282-0

Semantics that fall out of the format and drive the parser:

  * A rename of a surviving file is rendered as link + unlink (new
    name linked to the inode, old name unlinked), NOT as a rename
    command. Directory moves ARE rename commands. Newly created
    inodes first appear under kernel staging names
    (o<ino>-<transid>-0) and are renamed into place. A deleted
    directory is first renamed to a staging name, its children are
    unlinked under the staging prefix, and the staging name is
    rmdir'd: names in the stream are valid at their point in the
    stream, not in either snapshot.
  * dest= means three different things: for link, the existing tree
    path the inode is linked from (no ./ prefix); for rename, the
    new tree path (with ./ prefix); for symlink, the link target
    (arbitrary string, not a tree path at all).
  * Content changes appear as update_extent (stream v2) / write /
    clone / truncate / fallocate commands, never as data in
    --no-data mode. clone's tail is
    "offset=.. len=.. from=<source path> clone_offset=..".
  * set_xattr embeds the raw value bytes: a value containing a
    newline byte splits the rendered command across physical lines.
    The parser reassembles logical lines (a physical line not
    starting with a known command word continues the previous one)
    and validates the value's byte length against the len= field.

Because stream-time names shift as renames execute, the parser keeps
the operations in stream order and resolves every name through the
rename sequence: final_name() applies later renames forward,
retained_name() applies earlier renames in reverse. On top of that:

  * Operations on a multiply-linked file are expanded to every fresh
    path of that inode (the kernel names one representative path;
    the change applies to the inode, hence to all its names).
  * A moved directory contributes its whole subtree (old paths
    deleted, corresponding new paths changed); a deleted directory
    likewise contributes its subtree as deletions.
  * Final sets are intersected with tree reality: changed only
    contains fresh paths, deleted only retained-but-not-fresh paths
    that were not recreated.

List contract produced here (consumed by the Phase B patch mode):

  changed: paths present in the fresh tree that must be archived
    fresh (created, content- or metadata-modified, rename/link
    targets). Rename = changed at the new path.
  deleted: paths present in the retained tree and absent from the
    fresh tree. A path unlinked and recreated in the same interval
    is changed, not deleted (the patch replaces its node).

Known prototype limitations (the reason the brief reserves a TLV
parser for production): the text format separates fields by
whitespace and escapes special characters, so a path containing an
argument-like substring (" mode=", " dest=" as literal text) can be
ambiguous. dest= splits are validated against the two tree walks and
ambiguity is reported as an event, never silently resolved.
"""

import os
import re
import subprocess
import sys
import time

TEMP_RE = re.compile(r"^o\d+-\d+-\d+$")
CREATE_CMDS = {"mkfile", "mkdir", "mknod", "mkfifo", "mksock", "symlink"}
CONTENT_CMDS = {"update_extent", "write", "clone", "fallocate", "truncate"}
META_CMDS = {"chmod", "chown", "utimes", "set_xattr", "remove_xattr"}
DELETE_CMDS = {"unlink", "rmdir"}
KNOWN_CMDS = CREATE_CMDS | CONTENT_CMDS | META_CMDS | DELETE_CMDS | {
    "link", "rename", "snapshot", "subvol"}

_ARG_TAIL = {
    "chmod": re.compile(r"\s+mode=\S+$"),
    "chown": re.compile(r"\s+gid=\S+\s+uid=\S+$"),
    "utimes": re.compile(r"\s+atime=\S+\s+mtime=\S+\s+ctime=\S+$"),
    "update_extent": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    "write": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    "truncate": re.compile(r"\s+size=\S+$"),
    "remove_xattr": re.compile(r"\s+name=\S+$"),
    "fallocate": re.compile(r"\s+offset=\S+\s+len=\S+$"),
    # clone (stream v2): offset/len on the destination, then
    # from=<source path> clone_offset=<n>; the source path may
    # contain escaped spaces, hence the greedy match.
    "clone": re.compile(
        r"\s+offset=\S+\s+len=\S+\s+from=.*\s+clone_offset=\S+$"),
}


def _is_temp(path):
    return bool(TEMP_RE.match(path.rsplit("/", 1)[-1]))


def _unescape(s):
    """Undo the dump's backslash escaping of path bytes."""
    out = []
    i = 0
    while i < len(s):
        c = s[i]
        if c == "\\" and i + 1 < len(s):
            n = s[i + 1]
            if n == " ":
                out.append(" ")
                i += 2
            elif n == "\\":
                out.append("\\")
                i += 2
            elif n == "n":
                out.append("\n")
                i += 2
            elif n == "t":
                out.append("\t")
                i += 2
            elif n == "r":
                out.append("\r")
                i += 2
            elif n == "x" and i + 3 < len(s) + 1 and \
                    all(ch in "0123456789abcdefABCDEF"
                        for ch in s[i + 2:i + 4]):
                out.append(chr(int(s[i + 2:i + 4], 16)))
                i += 4
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


class DumpParser:
    def __init__(self, snap_name, old_records, new_records):
        self.snap = snap_name
        self.prefix = "./" + snap_name + "/"
        self.old = old_records
        self.new = new_records
        self.events = []
        self.ops = []       # (seq, kind, path, aux) in stream order
        self.renames = []   # (seq, src, dst) stream-time names
        self.new_ino = {}
        for p, r in new_records.items():
            if r["type"] == "file":
                self.new_ino.setdefault(r["ino"], []).append(p)
        self.old_ino = {}
        for p, r in old_records.items():
            if r["type"] == "file":
                self.old_ino.setdefault(r["ino"], []).append(p)

    # -- lexical layer -----------------------------------------------
    def _strip(self, raw):
        """Remove the ./<snap>/ prefix; the snapshot root becomes ""
        and already-relative names (link dest form) pass through."""
        if raw == self.prefix.rstrip("/"):
            return ""
        if raw.startswith(self.prefix):
            return raw[len(self.prefix):]
        if raw == "./" + self.snap:
            return ""
        return raw

    def _name(self, raw):
        return _unescape(self._strip(raw.strip()))

    def _split_dest(self, rest, cmd):
        splits = []
        start = 0
        while True:
            i = rest.find(" dest=", start)
            if i < 0:
                break
            splits.append((rest[:i].rstrip(), rest[i + 6:]))
            start = i + 1
        if not splits:
            self.events.append({"kind": "dest-missing", "cmd": cmd,
                                "line": rest})
            return rest, ""
        valid = []
        for path, value in splits:
            sp = self._name(path)
            if cmd == "link":
                sv = self._name(value)
                if sv in self.old or sv in self.new or _is_temp(sv):
                    valid.append((sp, sv))
            elif cmd == "rename":
                sv = self._name(value)
                if sv in self.new or _is_temp(sv):
                    valid.append((sp, sv))
            elif cmd == "symlink":
                if _is_temp(sp):
                    valid.append((sp, _unescape(value)))
        if len(valid) == 1:
            return valid[0]
        if cmd == "symlink" and splits:
            for path, value in reversed(splits):
                sp = self._name(path)
                if sp in self.new:
                    return sp, _unescape(value)
            p, v = splits[-1]
            return self._name(p), _unescape(v)
        if valid:
            self.events.append({"kind": "dest-ambiguous", "cmd": cmd,
                                "line": rest,
                                "candidates": [list(v) for v in valid]})
            return valid[0]
        p, v = splits[0]
        self.events.append({"kind": "dest-unvalidated", "cmd": cmd,
                            "line": rest})
        return self._name(p), self._name(v) if cmd != "symlink" else \
            _unescape(v)

    def _extract_path(self, cmd, rest):
        if cmd == "set_xattr":
            m = re.match(r"^(.*)\s+name=(\S+)\s+data=(.*)\s+len=(\d+)$",
                         rest, re.DOTALL)
            if m and len(m.group(3).encode(
                    "utf-8", "surrogateescape")) == int(m.group(4)):
                return m.group(1)
            self.events.append({"kind": "setxattr-unparsed",
                                "line": rest})
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

    # -- parse into ordered ops ---------------------------------------
    def _logical_lines(self, dump_text):
        """Reassemble physical lines into command lines. The dump
        embeds raw xattr values, which may contain newline bytes and
        so spill a set_xattr command across physical lines. A
        physical line that does not begin with a known command word
        (or the At subvol / snapshot headers) is a continuation of
        the previous line. Split strictly on \\n: splitlines() would
        also break on other unicode line boundaries that arbitrary
        bytes can decode to."""
        logical = []
        for phys in dump_text.split("\n"):
            first = phys.split(None, 1)[0] if phys.strip() else ""
            if first in KNOWN_CMDS or phys.startswith("At subvol"):
                logical.append(phys)
            elif logical:
                logical[-1] += "\n" + phys
            elif phys.strip():
                self.events.append({"kind": "orphan-continuation",
                                    "line": phys})
        return logical

    def parse(self, dump_text):
        seq = 0
        for line in self._logical_lines(dump_text):
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
            seq += 1
            if cmd == "rename":
                src, dst = self._split_dest(rest, cmd)
                self.ops.append((seq, "rename", src, dst))
                self.renames.append((seq, src, dst))
            elif cmd == "link":
                path, src = self._split_dest(rest, cmd)
                self.ops.append((seq, "link", path, src))
            elif cmd == "symlink":
                path, _target = self._split_dest(rest, cmd)
                self.ops.append((seq, "create", path, None))
            elif cmd in DELETE_CMDS:
                self.ops.append((seq, "delete",
                                 self._name(rest), None))
            elif cmd in CREATE_CMDS:
                self.ops.append((seq, "create",
                                 self._name(self._extract_path(cmd, rest)),
                                 None))
            else:
                self.ops.append((seq, "modify",
                                 self._name(self._extract_path(cmd, rest)),
                                 None))
        return self._resolve()

    # -- name resolution through the rename sequence ------------------
    def _final_name(self, p, seq):
        for (s, src, dst) in self.renames:
            if s <= seq:
                continue
            if p == src:
                p = dst
            elif src and p.startswith(src + "/"):
                p = dst + p[len(src):]
        return p

    def _retained_name(self, p, seq):
        for (s, src, dst) in reversed(self.renames):
            if s >= seq:
                continue
            if p == dst:
                p = src
            elif dst and p.startswith(dst + "/"):
                p = src + p[len(dst):]
        return p

    # -- resolution ----------------------------------------------------
    def _resolve(self):
        changed = set()
        deleted = set()
        moves = []  # (old_identity, final_identity) directory moves

        for (seq, kind, path, aux) in self.ops:
            if kind in ("create", "modify"):
                f = self._final_name(path, seq)
                if f in self.new:
                    changed.add(f)
                elif not _is_temp(f) and not _is_temp(path):
                    self.events.append({"kind": "op-path-unresolved",
                                        "op": kind, "path": path,
                                        "final": f})
            elif kind == "link":
                f = self._final_name(path, seq)
                if f in self.new:
                    changed.add(f)
                elif not _is_temp(f):
                    self.events.append({"kind": "op-path-unresolved",
                                        "op": kind, "path": path,
                                        "final": f})
            elif kind == "delete":
                o = self._retained_name(path, seq)
                f = self._final_name(path, seq)
                if o in self.old and f not in self.new:
                    deleted.add(o)
            elif kind == "rename":
                o = self._retained_name(path, seq)
                f = self._final_name(aux, seq)
                if o in self.old:
                    if f not in self.new:
                        deleted.add(o)
                    elif f != o:
                        deleted.add(o)
                        changed.add(f)
                        if self.old[o]["type"] == "dir":
                            moves.append((o, f))

        # Subtree effects of directory moves and deletions.
        old_children = {}
        for p in self.old:
            old_children.setdefault(p.rpartition("/")[0], []).append(p)

        def subtree_old(dirpath):
            out = []
            stack = [dirpath]
            while stack:
                d = stack.pop()
                for c in old_children.get(d, []):
                    out.append(c)
                    if self.old[c]["type"] == "dir":
                        stack.append(c)
            return out

        for (o, f) in moves:
            for c in subtree_old(o):
                if c not in self.new and c not in changed:
                    deleted.add(c)
                mapped = f + c[len(o):]
                if mapped in self.new:
                    changed.add(mapped)
        for d in list(deleted):
            if d in self.old and self.old[d]["type"] == "dir":
                for c in subtree_old(d):
                    if c not in self.new and c not in changed:
                        deleted.add(c)

        # Hardlink sibling expansion: an operation on one name of a
        # multiply-linked inode applies to all its fresh names; an op
        # path that names a retained location resolves through the
        # inode to its fresh names.
        expanded = set()
        for p in changed:
            expanded.add(p)
            if self.new[p]["type"] == "file":
                expanded.update(self.new_ino.get(self.new[p]["ino"], []))
        changed = {p for p in expanded if p in self.new}
        deleted = {p for p in deleted
                   if p in self.old and p not in self.new
                   and p not in changed}
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
    with open(dump, "wb") as f:
        subprocess.run(["btrfs", "receive", "--dump", "-f", stream],
                       check=True, stdout=f, stderr=subprocess.PIPE)
    t3 = time.time()
    # The dump embeds raw filename and xattr-value bytes; decode with
    # surrogateescape so byte-exact names round-trip, matching how
    # os.scandir surfaces them on the ground-truth side.
    with open(dump, encoding="utf-8", errors="surrogateescape") as f:
        text = f.read()
    return text, {"snapshot_s": t1 - t0, "send_s": t2 - t1,
                  "dump_s": t3 - t2, "stream_bytes": os.path.getsize(stream)}
