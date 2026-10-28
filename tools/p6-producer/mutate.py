#!/usr/bin/env python3
"""Fixture builders and mutation classes for the Phase A differential.

Every mutation operates on the LIVE tree only. Ground truth is always
derived independently by treediff.walk() on the two snapshots, never
from what this module believes it did; the op log exists purely for
diagnostics and per-class accounting.
"""

import os
import random
import shutil
import subprocess

WORDS = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot",
         "golf", "hotel", "india", "juliet", "kilo", "lima"]


def _write(path, data):
    with open(path, "wb") as f:
        f.write(data)


def _rand_bytes(rng, n):
    return rng.randbytes(n) if hasattr(rng, "randbytes") else \
        bytes(rng.getrandbits(8) for _ in range(n))


# ---------------------------------------------------------------------
# Fixture builders
# ---------------------------------------------------------------------

def build_fixture(root, seed=20261026):
    """Moderate fixture: ~500 files, nesting, hardlink groups (same-dir
    and cross-dir, one triple), symlinks (relative/absolute/dangling),
    fifos, xattrs, names with spaces and non-ASCII, pre-aged mtimes."""
    rng = random.Random(seed)
    dirs = ["", "dirA", "dirA/sub", "dirA/sub/deep", "dirB", "dirB/inner",
            "dirC with space", "empty-dir"]
    for i in range(30):
        dirs.append(f"bulk/d{i:02d}")
    for d in dirs:
        os.makedirs(os.path.join(root, d), exist_ok=True)
    n = 0
    for d in dirs:
        if d == "empty-dir":
            continue
        for i in range(rng.randint(4, 14)):
            name = f"f{i:02d}-{rng.choice(WORDS)}.bin"
            p = os.path.join(root, d, name)
            size = rng.choice([0, 1, 13, 100, 4096, 65536])
            _write(p, _rand_bytes(rng, size))
            n += 1
            if i == 0:
                os.utime(p, ns=(1234567890000000000, 1234567890000000000))
    _write(os.path.join(root, "one-meg.bin"), _rand_bytes(rng, 1 << 20))
    _write(os.path.join(root, "with space.txt"), b"space name\n")
    _write(os.path.join(root, "ünïcodé ✓.txt"), b"unicode name\n")
    # hardlink groups
    g1 = os.path.join(root, "dirA", "group1.bin")
    _write(g1, _rand_bytes(rng, 2048))
    os.link(g1, os.path.join(root, "dirA", "group1-link.bin"))
    os.link(g1, os.path.join(root, "dirB", "group1-xdir.bin"))
    g2 = os.path.join(root, "dirB", "group2.bin")
    _write(g2, _rand_bytes(rng, 777))
    os.link(g2, os.path.join(root, "dirB", "group2-link.bin"))
    # symlinks
    os.symlink("dirA/f00-alpha.bin", os.path.join(root, "rel-link"))
    os.symlink(os.path.join(root, "dirB"), os.path.join(root, "abs-link"))
    os.symlink("no/such/target", os.path.join(root, "dangling-link"))
    os.symlink("../dirB", os.path.join(root, "dirA", "sub", "up-link"))
    # fifos
    os.mkfifo(os.path.join(root, "dirA", "pipe1"))
    os.mkfifo(os.path.join(root, "fifo root"))
    # xattrs
    for p in ["dirA", "dirB", "one-meg.bin", "dirA/group1.bin"]:
        os.setxattr(os.path.join(root, p), "user.fixture", b"v1")
    os.setxattr(os.path.join(root, "dirA", "group1.bin"),
                "user.second", b"two")
    return n


def build_random_tree(root, seed, nfiles=2000, ndirs=150):
    rng = random.Random(seed)
    dirs = [""]
    for i in range(ndirs):
        d = f"g{i:03d}"
        os.makedirs(os.path.join(root, d), exist_ok=True)
        dirs.append(d)
        if i % 10 == 0:
            sub = f"g{i:03d}/nested"
            os.makedirs(os.path.join(root, sub), exist_ok=True)
            dirs.append(sub)
    for i in range(nfiles):
        d = rng.choice(dirs)
        p = os.path.join(root, d, f"f{i:05d}.dat")
        _write(p, _rand_bytes(rng, rng.choice([0, 1, 64, 1024, 8192])))
    # hardlink and symlink material for the random ops to find
    for i in range(0, nfiles, 200):
        src = os.path.join(root, f"f{i:05d}.dat")
        # locate the file (it is in a random dir; find via walk once)
    for dirpath, _dn, fn in os.walk(root):
        for name in fn:
            if name in ("f00000.dat", "f00200.dat", "f00400.dat"):
                full = os.path.join(dirpath, name)
                os.link(full, full + ".hl")
    os.symlink("g000", os.path.join(root, "dir-link"))
    return nfiles


# ---------------------------------------------------------------------
# Inventory (guides op selection only; never used as ground truth)
# ---------------------------------------------------------------------

def inventory(root):
    inv = {"dirs": [], "files": [], "symlinks": [], "fifos": [],
           "hardlinked": []}
    for dirpath, dirnames, filenames in os.walk(root):
        rel = os.path.relpath(dirpath, root)
        rel = "" if rel == "." else rel
        inv["dirs"].append(rel)
        for name in filenames:
            full = os.path.join(dirpath, name)
            rp = name if rel == "" else rel + "/" + name
            try:
                st = os.lstat(full)
            except OSError:
                continue
            if os.path.islink(full):
                inv["symlinks"].append(rp)
            elif os.path.isdir(full):
                pass
            elif name.endswith(".dat") or True:
                if st.st_nlink > 1:
                    inv["hardlinked"].append(rp)
                inv["files"].append(rp)
        for name in dirnames:
            full = os.path.join(dirpath, name)
            if os.path.islink(full):
                rp = name if rel == "" else rel + "/" + name
                inv["symlinks"].append(rp)
    return inv


# ---------------------------------------------------------------------
# Mutation ops: each takes (root, rng, inv, log) and applies 1 op.
# ---------------------------------------------------------------------

def _p(root, rel):
    return os.path.join(root, rel)


def _fresh_name(rng, inv, suffix=".bin"):
    d = rng.choice(inv["dirs"])
    name = f"new-{rng.randrange(10**8):08d}{suffix}"
    return name if d == "" else d + "/" + name


def op_create_file(root, rng, inv, log):
    rel = _fresh_name(rng, inv)
    _write(_p(root, rel), _rand_bytes(rng, rng.choice([0, 1, 100, 5000])))
    log.append(("create_file", rel))


def op_create_dir(root, rng, inv, log):
    rel = _fresh_name(rng, inv, suffix="")
    os.makedirs(_p(root, rel))
    log.append(("create_dir", rel))


def op_modify_append(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    with open(_p(root, rel), "ab") as f:
        f.write(_rand_bytes(rng, rng.choice([1, 50, 4096])))
    log.append(("modify_append", rel))


def op_modify_rewrite(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    st = os.lstat(_p(root, rel))
    _write(_p(root, rel), _rand_bytes(rng, st.st_size))
    log.append(("modify_rewrite", rel))


def op_modify_preserve_mtime(root, rng, inv, log):
    rel = rng.choice([f for f in inv["files"]
                      if os.lstat(_p(root, f)).st_size > 16]
                     or inv["files"])
    full = _p(root, rel)
    st = os.lstat(full)
    _write(full, _rand_bytes(rng, st.st_size))
    os.utime(full, ns=(st.st_atime_ns, st.st_mtime_ns))
    log.append(("modify_preserve_mtime", rel))


def op_modify_truncate(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    with open(_p(root, rel), "r+b") as f:
        f.truncate(rng.choice([0, 1, 10]))
    log.append(("modify_truncate", rel))


def op_delete_file(root, rng, inv, log):
    rel = rng.choice(inv["files"] + inv["symlinks"] + inv["fifos"])
    os.unlink(_p(root, rel))
    log.append(("delete_file", rel))


def op_delete_tree(root, rng, inv, log):
    cands = [d for d in inv["dirs"] if d and "/" in d]
    if not cands:
        return
    rel = rng.choice(cands)
    shutil.rmtree(_p(root, rel))
    log.append(("delete_tree", rel))


def op_rename_within(root, rng, inv, log):
    rel = rng.choice(inv["files"] + inv["symlinks"])
    d, _, name = rel.rpartition("/")
    new = (d + "/" if d else "") + "ren-" + name
    os.rename(_p(root, rel), _p(root, new))
    log.append(("rename_within", rel + " -> " + new))


def op_rename_across(root, rng, inv, log):
    rel = rng.choice(inv["files"] + inv["symlinks"])
    dest_dir = rng.choice([d for d in inv["dirs"]])
    name = rel.rpartition("/")[2]
    new = (dest_dir + "/" if dest_dir else "") + "x-" + name
    if new == rel:
        return
    os.rename(_p(root, rel), _p(root, new))
    log.append(("rename_across", rel + " -> " + new))


def op_rename_dir(root, rng, inv, log):
    cands = [d for d in inv["dirs"] if d]
    rel = rng.choice(cands)
    parent, _, name = rel.rpartition("/")
    new = (parent + "/" if parent else "") + "rd-" + name
    os.rename(_p(root, rel), _p(root, new))
    log.append(("rename_dir", rel + " -> " + new))


def op_rename_swap(root, rng, inv, log):
    a, b = rng.sample(inv["files"], 2)
    tmp = _fresh_name(rng, inv, suffix=".swap")
    os.rename(_p(root, a), _p(root, tmp))
    os.rename(_p(root, b), _p(root, a))
    os.rename(_p(root, tmp), _p(root, b))
    log.append(("rename_swap", a + " <-> " + b))


def op_hardlink_add(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    new = _fresh_name(rng, inv, suffix=".hl")
    os.link(_p(root, rel), _p(root, new))
    log.append(("hardlink_add", rel + " => " + new))


def op_hardlink_remove(root, rng, inv, log):
    if not inv["hardlinked"]:
        return
    rel = rng.choice(inv["hardlinked"])
    os.unlink(_p(root, rel))
    log.append(("hardlink_remove", rel))


def op_symlink_create(root, rng, inv, log):
    rel = _fresh_name(rng, inv, suffix=".lnk")
    target = rng.choice(inv["files"] + ["missing-target", "../"])
    os.symlink(target, _p(root, rel))
    log.append(("symlink_create", rel))


def op_symlink_retarget(root, rng, inv, log):
    if not inv["symlinks"]:
        return
    rel = rng.choice(inv["symlinks"])
    full = _p(root, rel)
    if os.path.isdir(full) and not os.path.islink(full):
        return
    os.unlink(full)
    os.symlink(rng.choice(["target-a", "target-b", "/abs/target"]),
               full)
    log.append(("symlink_retarget", rel))


def _other_mode(rng, full):
    cur = os.lstat(full).st_mode & 0o777
    if os.path.isdir(full):
        # Directory modes in the differential must keep owner
        # read+execute: a non-traversable directory (0444, 0600)
        # blinds the userspace ground-truth walker (EACCES on child
        # stat), while the kernel send stream is unaffected, which
        # fabricates divergences. restic in production runs as root
        # and archives such directories fine; the producer mechanism
        # is mode-agnostic, so this scoping costs the gate nothing.
        pool = (0o700, 0o755, 0o500, 0o751)
    else:
        pool = (0o600, 0o644, 0o755, 0o444, 0o640)
    choices = [m for m in pool if m != cur]
    return rng.choice(choices)


def op_chmod_only(root, rng, inv, log):
    rel = rng.choice(inv["files"] + inv["dirs"])
    os.chmod(_p(root, rel), _other_mode(rng, _p(root, rel)))
    log.append(("chmod_only", rel))


def op_utimes_only(root, rng, inv, log):
    rel = rng.choice(inv["files"] + inv["dirs"])
    ns = rng.randrange(10**17, 17 * 10**17)
    os.utime(_p(root, rel), ns=(ns, ns))
    log.append(("utimes_only", rel))


def op_atime_only(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    st = os.lstat(_p(root, rel))
    os.utime(_p(root, rel),
             ns=(st.st_atime_ns + 10**12, st.st_mtime_ns))
    log.append(("atime_only", rel))


def op_xattr_change(root, rng, inv, log):
    rel = rng.choice(inv["files"] + [d for d in inv["dirs"] if d])
    full = _p(root, rel)
    if rng.random() < 0.5:
        os.setxattr(full, "user.mut", _rand_bytes(rng, 8))
        log.append(("xattr_set", rel))
    else:
        try:
            os.removexattr(full, "user.fixture")
            log.append(("xattr_remove", rel))
        except OSError:
            os.setxattr(full, "user.mut", b"fallback")
            log.append(("xattr_set", rel))


def op_type_file_to_dir(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    full = _p(root, rel)
    os.unlink(full)
    os.mkdir(full)
    log.append(("type_file_to_dir", rel))


def op_type_file_to_symlink(root, rng, inv, log):
    rel = rng.choice(inv["files"])
    full = _p(root, rel)
    os.unlink(full)
    os.symlink("was-a-file", full)
    log.append(("type_file_to_symlink", rel))


def op_type_dir_to_file(root, rng, inv, log):
    cands = [d for d in inv["dirs"] if d and "/" in d]
    if not cands:
        return
    rel = rng.choice(cands)
    full = _p(root, rel)
    shutil.rmtree(full)
    _write(full, b"was-a-dir")
    log.append(("type_dir_to_file", rel))


def op_reflink_copy(root, rng, inv, log):
    rel = rng.choice([f for f in inv["files"]
                      if os.lstat(_p(root, f)).st_size > 0]
                     or inv["files"])
    new = _fresh_name(rng, inv, suffix=".reflink")
    subprocess.run(["cp", "--reflink=always", _p(root, rel),
                    _p(root, new)], check=True)
    log.append(("reflink_copy", rel + " => " + new))


def op_modify_hardlinked(root, rng, inv, log):
    if not inv["hardlinked"]:
        return
    rel = rng.choice(inv["hardlinked"])
    with open(_p(root, rel), "ab") as f:
        f.write(b"hl-mod")
    log.append(("modify_hardlinked", rel))


def op_metadata_storm(root, rng, inv, log):
    # Pick a directory that actually contains files (a storm over an
    # empty directory tests nothing).
    cands = []
    for d in inv["dirs"]:
        if not d:
            continue
        n = sum(len(fn) for _dp, _dn, fn in os.walk(_p(root, d)))
        if n >= 3:
            cands.append(d)
    if not cands:
        log.append(("metadata_storm", "skipped:no-populated-dir"))
        return
    rel = rng.choice(cands)
    for dirpath, _dn, fn in os.walk(_p(root, rel)):
        for name in fn:
            full = os.path.join(dirpath, name)
            if os.path.islink(full):
                continue
            os.chmod(full, _other_mode(rng, full))
    log.append(("metadata_storm", rel))


OLD_NS = 1_000_000_000_000_000_000  # 2001-09-09; stale under relatime


def _subtree_paths(root, rel):
    """(dirs, files) under rel (rel itself included in dirs),
    symlinks excluded from files."""
    dirs, files = [], []
    for dirpath, _dn, filenames in os.walk(_p(root, rel)):
        rp = os.path.relpath(dirpath, root)
        rp = "" if rp == "." else rp
        dirs.append(rp)
        for name in filenames:
            full = os.path.join(dirpath, name)
            if os.path.islink(full):
                continue
            files.append(name if not rp else rp + "/" + name)
    return dirs, files


def _age_atimes(root, paths):
    for rp in paths:
        full = _p(root, rp)
        try:
            st = os.lstat(full)
        except OSError:
            continue
        os.utime(full, ns=(OLD_NS, st.st_mtime_ns))


def op_atime_sweep(root, rng, inv, log):
    # Mass touch -a: every file and directory under one subtree
    # gets a fresh atime; no mtime moves.
    cands = [d for d in inv["dirs"] if d]
    if not cands:
        return
    rel = rng.choice(cands)
    dirs, files = _subtree_paths(root, rel)
    n = 0
    for rp in dirs + files:
        full = _p(root, rp)
        try:
            st = os.lstat(full)
        except OSError:
            continue
        os.utime(full, ns=(st.st_atime_ns + 10**12, st.st_mtime_ns))
        n += 1
    log.append(("atime_sweep", f"{rel} ({n} paths)"))


def op_readdir_sweep(root, rng, inv, log):
    # Age every atime in a subtree, then readdir-walk it: under
    # relatime the directory atimes refresh; nothing else moves.
    cands = [d for d in inv["dirs"] if d]
    if not cands:
        return
    rel = rng.choice(cands)
    dirs, files = _subtree_paths(root, rel)
    _age_atimes(root, dirs + files)

    def walk(d):
        with os.scandir(d) as it:
            entries = list(it)
        for e in entries:
            if e.is_dir(follow_symlinks=False):
                walk(e.path)
    walk(_p(root, rel))
    log.append(("readdir_sweep", rel))


def op_read_sweep(root, rng, inv, log):
    # Age every file atime in a subtree, then read every file:
    # under relatime the file atimes refresh; contents untouched.
    cands = [d for d in inv["dirs"] if d]
    if not cands:
        return
    rel = rng.choice(cands)
    _dirs, files = _subtree_paths(root, rel)
    _age_atimes(root, files)
    n = 0
    for rp in files:
        with open(_p(root, rp), "rb") as f:
            while f.read(1 << 16):
                pass
        n += 1
    log.append(("read_sweep", f"{rel} ({n} files)"))


def op_atime_mixed(root, rng, inv, log):
    # Three neighbours, three fates: content-modified AND atime-
    # touched (must stay changed), atime-touched only (must drop),
    # atime and mtime both moved (must stay changed).
    a, b, c = rng.sample(inv["files"], 3)
    with open(_p(root, a), "ab") as f:
        f.write(b"mixed")
    st = os.lstat(_p(root, a))
    os.utime(_p(root, a), ns=(st.st_atime_ns + 10**12, st.st_mtime_ns))
    st = os.lstat(_p(root, b))
    os.utime(_p(root, b), ns=(st.st_atime_ns + 10**12, st.st_mtime_ns))
    ns = rng.randrange(10**17, 17 * 10**17)
    os.utime(_p(root, c), ns=(ns, ns))
    log.append(("atime_mixed", f"{a}|{b}|{c}"))


def op_new_file_utime(root, rng, inv, log):
    # A genuinely new file that also receives an atime touch
    # before the snapshot: it must stay in the changed set (the
    # retained tree holds no path the filter could prove from).
    rel = _fresh_name(rng, inv)
    _write(_p(root, rel), _rand_bytes(rng, 64))
    st = os.lstat(_p(root, rel))
    os.utime(_p(root, rel),
             ns=(st.st_atime_ns + 10**12, st.st_mtime_ns))
    log.append(("new_file_utime", rel))


RANDOM_OPS = [
    (op_create_file, 14), (op_create_dir, 4), (op_modify_append, 14),
    (op_modify_rewrite, 8), (op_modify_preserve_mtime, 4),
    (op_modify_truncate, 5), (op_delete_file, 10),
    (op_delete_tree, 2), (op_rename_within, 7), (op_rename_across, 7),
    (op_rename_dir, 3), (op_rename_swap, 2), (op_hardlink_add, 5),
    (op_hardlink_remove, 3), (op_symlink_create, 4),
    (op_symlink_retarget, 3), (op_chmod_only, 6), (op_utimes_only, 5),
    (op_atime_only, 3), (op_xattr_change, 6),
    (op_type_file_to_dir, 2), (op_type_file_to_symlink, 2),
    (op_type_dir_to_file, 1), (op_reflink_copy, 3),
    (op_modify_hardlinked, 3), (op_metadata_storm, 1),
    (op_atime_sweep, 2), (op_readdir_sweep, 2), (op_read_sweep, 2),
    (op_atime_mixed, 2), (op_new_file_utime, 3),
]


def apply_random_batch(root, rng, n_ops, oplog):
    weights = [w for _f, w in RANDOM_OPS]
    funcs = [f for f, _w in RANDOM_OPS]
    for _ in range(n_ops):
        inv = inventory(root)
        fn = rng.choices(funcs, weights=weights, k=1)[0]
        try:
            fn(root, rng, inv, oplog)
        except (OSError, IndexError, ValueError, subprocess.CalledProcessError) as e:
            oplog.append(("op-failed:" + fn.__name__, repr(e)))


# Scripted per-class sequences for the matrix phase: each entry is a
# function applying a small deterministic burst of one class.
def matrix_cases():
    return {
        "create": [op_create_file, op_create_file, op_create_dir],
        "content_modify": [op_modify_append, op_modify_rewrite,
                           op_modify_truncate],
        "modify_preserve_mtime": [op_modify_preserve_mtime] * 3,
        "delete_file": [op_delete_file] * 3,
        "delete_tree": [op_delete_tree],
        "rename_within": [op_rename_within] * 3,
        "rename_across": [op_rename_across] * 3,
        "rename_dir": [op_rename_dir] * 2,
        "rename_swap": [op_rename_swap] * 2,
        "mkdir_rmdir": [op_create_dir, op_create_dir],
        "hardlink_add": [op_hardlink_add] * 3,
        "hardlink_remove": [op_hardlink_remove] * 2,
        "modify_hardlinked": [op_modify_hardlinked] * 2,
        "symlink_create": [op_symlink_create] * 3,
        "symlink_retarget": [op_symlink_retarget] * 3,
        "chmod_only": [op_chmod_only] * 4,
        "utimes_only": [op_utimes_only] * 4,
        "atime_only": [op_atime_only] * 4,
        "atime_sweep": [op_atime_sweep] * 2,
        "readdir_sweep": [op_readdir_sweep] * 2,
        "read_sweep": [op_read_sweep] * 2,
        "atime_mixed": [op_atime_mixed] * 2,
        "new_file_utime": [op_new_file_utime] * 2,
        "xattr_change": [op_xattr_change] * 5,
        "type_file_to_dir": [op_type_file_to_dir] * 2,
        "type_file_to_symlink": [op_type_file_to_symlink] * 2,
        "type_dir_to_file": [op_type_dir_to_file],
        "reflink_copy": [op_reflink_copy] * 3,
        "metadata_storm": [op_metadata_storm],
        "empty_diff": [],
    }
