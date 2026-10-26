#!/usr/bin/env python3
"""Ground-truth tree comparison for the Project 6 producer differential.

This module walks two directory trees (in practice two read-only btrfs
snapshots of the same subvolume) and classifies every relative path as
changed, deleted, or unchanged, using ONLY filesystem observations.
It knows nothing about btrfs send streams; that independence is what
makes it a ground truth for the send-stream parser in sendparse.py.

Comparison definition (per relative path, "" denotes the tree root):

  A record is captured for every path: type, and for all types the
  lstat fields mode, uid, gid, mtime_ns, atime_ns, ctime_ns; for
  regular files also size, nlink, inode and a SHA-256 of the content;
  for symlinks the target string; for every type the xattr dict
  (names -> values, read with follow_symlinks=False). Directory
  st_size is deliberately NOT recorded: btrfs directory size is an
  accounting artefact, not content, and restic does not archive it.

  changed(path): the path exists in both trees and any compared field
    differs: type, file size, mtime_ns, mode, uid, gid, symlink
    target, xattr dict, or content hash. A type difference counts as
    changed (replace semantics), not as delete+create.
  created(path): exists only in the new tree. Reported as part of
    the changed set (patch mode archives it fresh).
  deleted(path): exists only in the old tree.

  atime_ns and ctime_ns are recorded but are NOT part of the changed
  rule: restic does not archive ctime, and atime is archived only
  under --with-atime. They exist so the harness can classify parser
  extras precisely: a parser-reported change where the two records
  differ ONLY in atime/ctime is a demonstrably harmless extra.

Nested subvolumes: a directory whose inode is 256 (a btrfs subvolume
root) is recorded as a leaf directory and not descended into. A
snapshot of the source appearing inside the tree therefore does not
pollute the comparison. (Phase A fixtures contain no nested
subvolumes; the guard exists for the corpus shapes.)
"""

import hashlib
import os
import stat
import sys

SUBVOL_ROOT_INO = 256


def _xattrs(path):
    try:
        names = os.listxattr(path, follow_symlinks=False)
    except OSError:
        return {}
    out = {}
    for name in names:
        try:
            out[name] = os.getxattr(path, name, follow_symlinks=False)
        except OSError:
            out[name] = None
    return out


def _hash_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def walk(root, hash_content=True):
    """Return {relpath: record} for the tree at root ("" is the root)."""
    records = {}

    def record(rel, full, st):
        mode = st.st_mode
        if stat.S_ISREG(mode):
            typ = "file"
        elif stat.S_ISDIR(mode):
            typ = "dir"
        elif stat.S_ISLNK(mode):
            typ = "symlink"
        elif stat.S_ISFIFO(mode):
            typ = "fifo"
        elif stat.S_ISSOCK(mode):
            typ = "socket"
        elif stat.S_ISCHR(mode):
            typ = "char"
        elif stat.S_ISBLK(mode):
            typ = "block"
        else:
            typ = "other"
        rec = {
            "type": typ,
            "mode": stat.S_IMODE(mode),
            "uid": st.st_uid,
            "gid": st.st_gid,
            "mtime_ns": st.st_mtime_ns,
            "atime_ns": st.st_atime_ns,
            "ctime_ns": st.st_ctime_ns,
            "ino": st.st_ino,
            "nlink": st.st_nlink,
        }
        if typ == "file":
            rec["size"] = st.st_size
            if hash_content:
                rec["sha256"] = _hash_file(full)
        elif typ == "symlink":
            rec["target"] = os.readlink(full)
        rec["xattrs"] = _xattrs(full)
        records[rel] = rec
        return typ

    root_st = os.lstat(root)
    record("", root, root_st)

    def descend(rel_dir, full_dir):
        try:
            entries = list(os.scandir(full_dir))
        except OSError as e:
            print(f"treediff: cannot scandir {full_dir}: {e}", file=sys.stderr)
            return
        for entry in entries:
            rel = entry.name if rel_dir == "" else rel_dir + "/" + entry.name
            try:
                st = entry.stat(follow_symlinks=False)
            except OSError as e:
                print(f"treediff: cannot stat {entry.path}: {e}", file=sys.stderr)
                continue
            typ = record(rel, entry.path, st)
            if typ == "dir" and st.st_ino != SUBVOL_ROOT_INO:
                descend(rel, entry.path)

    descend("", root)
    return records


_COMPARE_KEYS = ("type", "mode", "uid", "gid", "mtime_ns",
                 "size", "target", "xattrs", "sha256")


def diff(old, new):
    """Classify paths between two walk() record dicts.

    Returns (changed, deleted, detail) where changed includes created
    paths, and detail maps path -> list of differing keys (or a
    marker: 'created', 'deleted', 'atime/ctime-only' is NOT used
    here; a path whose only differing keys are atime_ns/ctime_ns is
    by definition absent from changed, and the harness detects that
    case from the records when classifying parser extras).
    """
    changed = set()
    deleted = set()
    detail = {}
    for path in old:
        if path not in new:
            deleted.add(path)
            detail[path] = ["deleted"]
    for path in new:
        if path not in old:
            changed.add(path)
            detail[path] = ["created"]
            continue
        o, n = old[path], new[path]
        keys = [k for k in _COMPARE_KEYS if o.get(k) != n.get(k)]
        if keys:
            changed.add(path)
            detail[path] = keys
    return changed, deleted, detail


def only_atime_ctime(old, new, path):
    """True if path exists in both and records differ at most in
    atime_ns/ctime_ns (plus nothing else)."""
    if path not in old or path not in new:
        return False
    o, n = old[path], new[path]
    keys = [k for k in _COMPARE_KEYS if o.get(k) != n.get(k)]
    return not keys and (o["atime_ns"] != n["atime_ns"]
                         or o["ctime_ns"] != n["ctime_ns"])
