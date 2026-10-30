# Producer v5: tree-search source for the atime filter's retained mtime

Branch `p6-tree-search` on the v4 tip (`p6-ino-fix`, 571a1cf).
Producer crate only. Nothing outside the crate changes: not the
wrapper, not the patch-mode restic, not the list formats, not the
manifest keys, not finalize.

## The problem

Production, 2026-10-30 night on stratos: prepare `diff_s` was
2,668 s. The send stream carried ~316k candidate paths, of which
314,838 were atime-only and dropped, leaving 729 changed /
167 deleted. Reading resolve.rs showed where the time goes:

- the op scan pays a fresh-tree pathname lstat (`in_new`) for
  every candidate before the filter runs, and
- the atime filter then pays a second pathname lstat in the
  retained tree (`meta_old`) to compare the stream's mtime
  against the retained snapshot's mtime for that path.

That is ~630k pathname stats, issued in the stream's inode
order (scattered across directories, so directory and inode
caches are cold at every step) against two snapshots on
spinning disks: ~8.5 ms per dropped candidate all-in. The send
itself is not the sink (OCI benchmark 2026-10-26: send
--no-data, 2,400 changes on the 200k corpus, 5.5 s).

## What the stream does and does not carry

Verified empirically on the testbed (kernel 6.17) with a TLV
probe of a real stream: a set_times command (cmd 20) carries
exactly four attributes: PATH (15), ATIME (11), MTIME (10),
CTIME (9), each timespec a little-endian i64 seconds + u32
nanoseconds. **There is no INO attribute on set_times** (nor on
the other path commands; INO appears on the snapshot framing
command). The stream therefore supplies the *fresh* atime,
mtime and ctime, and the parser already extracts the fresh
mtime into `Op.mtime`. What the stream never supplies is the
*retained* (parent snapshot) side of any comparison. The
atime-only verdict is exactly such a comparison:

    drop  <=>  every implicating op is set_times
               AND stream mtime == retained mtime (to the ns)

so the retained value must come from somewhere. In v3/v4 it
comes from a per-path lstat. In v5 it comes, in bulk, from the
retained snapshot's own metadata tree.

## The design

Before any candidate is proved, and only when the first
candidate actually needs a proof (lazy, so a churn-free night
pays nothing), the resolver builds one map:

    retained inode number  ->  (mtime sec, mtime nsec)

with a single `BTRFS_IOC_TREE_SEARCH_V2` walk over the retained
snapshot (new module `treesearch.rs`, following the raw-ioctl
idiom of `ino.rs` and the walk pattern of `internal/btrfs`,
whose gates validated it for Project 2). Settled facts that
constrain the implementation:

- The search key is one lexicographic (objectid, type, offset)
  range, so the walk spans all item types and filters
  client-side for INODE_ITEM (type 1).
- Decisions use fields inside the INODE_ITEM body, never the
  search header (the header transid is the leaf's COW stamp).
  INODE_ITEM body layout (160 bytes): generation @0, transid
  @8, size @16, nbytes @24, block_group @32, nlink @40, uid
  @44, gid @48, mode @52, rdev @56, flags @64, sequence @72,
  reserved @80..112, atime @112, ctime @124, **mtime @136**
  (i64 sec, u32 nsec at @144), otime @148. An item shorter
  than 160 bytes is not decoded (the inode is simply absent
  from the map, which fails toward inclusion via the fallback
  below).
- The walk uses tree_id 0 (the tree of the fd it is issued
  on, here an open handle on the retained snapshot root), a
  4 MiB result buffer per call, and advances the key past the
  last returned item exactly as `internal/btrfs` does.
- The producer runs as root in production (and under sudo in
  the gates), so the CAP_SYS_ADMIN the ioctl requires is
  present, as it already is for INO_PATHS.

An INODE_ITEM's mtime *is* the value an lstat of that inode in
that snapshot returns; the map is a faster source for the same
fact, not a new fact and not a new rule.

### The keying problem, and its solution

Utines ops are path-keyed and carry no inode, so a candidate
path must be linked to a retained inode before the map can be
consulted. The link is the candidate's **fresh inode**, taken
from `meta_new`, the fresh-tree lstat the resolver already
performs and caches for every changed-inode path during the
op scan and sibling expansion. No new filesystem access is
introduced for the link; the change *removes* the second
stat (the retained one) for map-covered candidates.

Why keying by the fresh inode is sound for a candidate:

- Retained and fresh are snapshots of one subvolume lineage,
  so they share one inode space: an inode number denotes the
  same object in both trees (snapshot creation preserves
  objectids).
- A *candidate* is by construction a path implicated only by
  set_times records: no create, unlink, link, rename, content
  or metadata op names it, it is not positionally changed, and
  no substantive op reached it through hardlink expansion.
  Any event that could swap the inode living at that path
  (delete + recreate, rename-over, rename-in) emits exactly
  those substantive ops for that path, by the completeness of
  the send stream. So for a genuine candidate, the inode at
  the path in the fresh tree is the inode that lived at the
  same path in the retained tree, and its retained mtime is
  the map value. Inode-number reuse across the interval
  cannot manufacture a candidate either: reuse at the same
  path entails the namespace ops above; reuse at a different
  path is irrelevant, because the lookup key is the
  candidate's own inode.
- Hardlinks: mtime is a per-inode fact, so the map value
  equals what `meta_old` would return for *every* name of the
  inode, including siblings reached by candidacy propagation
  (the v4 code stats the sibling's path and reads the same
  inode's mtime).
- Directories are INODE_ITEMs too and are in the map on the
  same footing as files.

Two classes are deliberately **not** served by the map:

- **Inode 256.** Every subvolume root has inode 256 in its
  own inode space, while the retained tree's map entry for
  256 is the retained snapshot root itself. A candidate for
  the snapshot root ("" path) or for a nested subvolume's
  root entry would key-collide. Candidates with fresh inode
  256 always take the stat proof.
- **Map misses.** Any candidate whose fresh inode is absent
  from the map (a truncated/short item, an inode in a nested
  subvolume's tree, which the retained root's search does not
  span, or any other gap) takes the stat proof. Note the send
  stream never names paths *inside* a nested subvolume (it
  treats the subvolume root as an empty directory), so the
  only nested case that can arise is the root entry itself,
  handled by the 256 rule and the miss rule together.

### The fallback ladder (fail toward inclusion, always)

For a candidate with stream mtime `S`:

1. If a map exists, the candidate's fresh inode is known and
   is not 256, and the map contains that inode: the verdict
   is `map[inode] == S`, positive or negative. This is the
   same comparison v4 makes against the stat result.
2. Otherwise (no map, map miss, inode 256, no fresh meta):
   the v4 proof, verbatim: `meta_old(path)` lstat in the
   retained tree, equal-to-the-ns or keep.
3. If building the map fails outright (ioctl error, a
   non-btrfs retained tree, insufficient privilege): there is
   no map, the run proceeds exactly as v4 for every candidate,
   and an `tree-search-unavailable` event is recorded. A
   successful build records a `tree-search-map` event with the
   entry count, mirroring the `ino-paths-clamped` event's use
   of the path field for diagnostics. Events are diagnostics
   only; no gate or wrapper key changes.

Nothing declines and nothing falls back a whole run: worst
case, v5 behaves byte-for-byte like v4.

## Performance shape and budget

`run_perfshape.py` (this crate) builds the shape the v3/v4
gates never had: the full 200k-file corpus reflink-copied into
a subvolume, a retained snapshot, an atime-only sweep over
every entry (files, directories, symlinks; atime set to now,
mtime preserved to the nanosecond), a handful of substantive
changes mixed in (rewrites, appends, mtime-only traps, chmod,
creates, deletes, a directory deletion, renames, a hardlink),
a fresh snapshot, and one spooled send stream. It records the
send-only wall time for the identical shape and machine, then
times `parse` for the v4 binary and the v5 binary against the
same stream and trees.

Budget, derived from the measured send-only time `S` on that
shape: **v5 parse wall time <= S + 120 s**, and the lists and
atime_only count from v5 must be identical to v4's on the same
stream. Rationale: the send is the irreducible kernel work of
the current architecture; a resolver whose remaining cost
(parse + one tree-search pass + the substantive set's stats)
is within a fixed two-minute allowance of it has removed the
per-candidate term that produced 2,668 s. The v4 parse time
on the same shape is recorded for contrast and carries no
assertion.

## Gates

Unchanged and unweakened: cargo unit tests; `run_noatime.py`
(the atime gate: matrix + random batches under the tightened
verdict, zero omissions, changed set exactly the substantive
set); `drills_phaseD.py` (Phase C drills); plus the new
performance shape above. The gate harnesses invoke parse under
sudo on the testbed, so the tree-search path is genuinely
exercised by the differential, not bypassed: a wrong map
would surface as omissions or extras against the treediff
ground truth.
