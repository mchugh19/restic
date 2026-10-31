# Producer v6: the ctime-invariant atime proof

Status: implemented on branch `p6-ctime-proof` (off the v4 tip,
`p6-ino-fix`). Producer crate only. This document describes what
the code does today: the proof, where its threshold comes from,
the fallback ladder, and the clock assumptions stated plainly.

## Why

The atime filter drops a changed-list path when the only thing
the send stream reports for it is access-time movement. v3/v4
prove that per candidate with one retained-tree lstat (the stat
proof below). v5 replaced those stats with a full TREE_SEARCH_V2
walk of the retained tree; on stratos's hard disks that walk
costs 13 to 34 minutes on churn nights, because it reads every
metadata leaf of a multi-million-inode tree to answer a question
about a handful of candidates. v6 removes the walk entirely and
proves most candidates from data the stream already carries,
with no retained-side lookup at all.

## The invariant

Linux timestamp semantics, for any inode:

- A read-driven atime update (the relatime/noatime machinery
  refreshing atime because the file or directory was read) does
  NOT change ctime.
- Every metadata change sets ctime to the wall-clock moment of
  the change: writes, utimensat setting any time field, chmod,
  chown, xattr set/remove, link and unlink (link-count changes),
  rename of the inode itself.
- Userspace cannot set ctime directly. There is no syscall for
  it; utimensat takes atime and mtime only.

So ctime is a lower bound on the last metadata-change moment,
and it can only move forward when metadata changes.

## The proof

A candidate is a path whose implicating stream operations are
all set_times records (the existing candidacy rules, unchanged:
no create, content, metadata, link or rename operation, no
positional change, hardlink siblings inherit candidacy).

Each set_times record carries the inode's NEW ctime, decoded by
the stream parser from the CTIME attribute (attribute 9) exactly
as the mtime is decoded from attribute 10.

Let T be the retained snapshot's creation time (the threshold,
below). For a candidate whose last set_times record carries
ctime C:

- If C < T (strict, nanosecond comparison): no metadata change
  can have occurred inside the interval (retained creation, run
  moment]. Any such change would have stamped ctime with a
  moment inside the interval, and ctime cannot move backwards
  or be set by hand. The only time field the op can be
  reporting movement in is therefore atime, so the path drops.
  No retained-side read of any kind happens for this candidate.
- If C >= T, or C is absent or undecodable: the ctime proof
  says nothing, and the candidate falls through to the stat
  proof.

Equality C == T falls through deliberately: a change stamped in
the retained snapshot's own creation instant is ambiguous (the
change could have raced the snapshot), and ambiguity always
fails toward the retained lookup, never toward a drop.

Per-inode exactness: ctime and mtime are both per-inode facts,
so when hardlink sibling expansion propagates a candidacy to
the inode's other names, the (mtime, ctime) pair travels with
it unchanged.

## The fallback ladder

1. ctime proof (above): drop, no retained lookup.
2. stat proof (the v3/v4 proof, verbatim): drop iff the
   record's mtime exists and equals the retained tree's mtime
   for the same path to the nanosecond (one lstat per candidate
   that reaches this rung).
3. Keep the path in the changed list.

Every doubt lands on a lower rung or on "keep": missing ctime,
ctime inside the interval, threshold unavailable, missing
stream mtime, path absent from the retained tree, mtimes apart.
The final changed and deleted lists are therefore the same
lists v4 produces, on every input; the gates assert this
verdict-for-verdict against a same-seeds v4 run.

## The threshold: kernel otime, read fresh each run

The threshold is the retained snapshot's ROOT_ITEM otime,
obtained with its uuid in ONE BTRFS_IOC_GET_SUBVOL_INFO call
(request 0x81f8943c, 504-byte result; uuid at offset 296, otime
seconds at 392, otime nanoseconds at 400) on an open handle of
the retained root, at the first candidate of the run, and
cached for the run. A snapshot's otime is its own creation
time, set by the kernel when the snapshot is taken; this was
verified empirically on the testbed (otime inside the
wall-clock window of the `btrfs subvolume snapshot` command,
nanosecond precision, uuid distinct from the source).

Why this source, over the alternative of producer-persisted
state (recording each fresh snapshot's creation wall time in
state.json beside its uuid and carrying it through finalize):

- Atomicity with identity: uuid and otime arrive in the same
  kernel result, about exactly the snapshot being diffed,
  whatever lifecycle path produced it. A persisted timestamp
  can be stale, mismatched or lost; this one cannot, because
  it is never stored at all.
- Precision: otime is the kernel's own freeze instant. A
  persisted wall time would have to be taken BEFORE the
  snapshot command runs to be sound (a timestamp taken after
  creation could sit after an early in-interval change's
  ctime and wrongly admit it), making it an approximation of
  the true threshold, never the value itself.
- Lifecycle surface: persisting means state-schema changes
  across every write_phase path (prepare, finalize, rebase,
  adoption), plus a separate channel for gates and drills to
  inject thresholds into parse runs. The ioctl needs none of
  that: init, adoption, re-base, fallback and plain runs all
  degrade or qualify identically, by construction.

Validation before the threshold is trusted (any failure
disables the ctime proof for the whole run; the stat proof
then carries every candidate, exactly as in v4):

- Both roots answer GET_SUBVOL_INFO with a decodable result
  (nonzero uuid, otime nanoseconds < 1e9). The ioctl returns 1
  on success on the kernels in use (errno 0, buffer filled);
  errors are -1. Non-btrfs trees (drills on plain directories,
  other filesystems) fail here and simply run stat-only.
- The two snapshots' uuids differ (a retained/fresh pair, not
  the same subvolume twice).
- retained.otime < fresh.otime, strictly. If the wall clock
  went backwards between the two creations (or cannot order
  them), the interval's clock is not trusted.
- retained.otime <= the run's own clock reading. A threshold
  in the future means the clock moved backwards by more than
  the interval, and nothing time-based is trusted that run.
- The drill hook P6_NO_CTIME_PROOF is not set (present in the
  environment forces disabled; the harness uses it to prove
  the disabled run equals v4 exactly).

Disabling is recorded as an aggregate event
(`ctime-proof-disabled`); a run with candidates and a live
threshold records `ctime-proof` with the count of
ctime-proven drops. Both ride in the parse summary's events
(the aggregate count travels in the event's path field,
mirroring the crate's existing count-bearing events); no other
event kind, key or format changed, and the wrapper consumes
the prepare manifest, not parse events.

## Clock assumptions, stated plainly

- One wall clock orders everything: ctimes are stamped by the
  (single) host kernel, the snapshot otime is stamped by the
  same kernel, and the run reads the same clock. Stratos and
  the testbed are single hosts; there is no cross-machine
  comparison anywhere in the proof.
- The clock does not step backwards inside a backup interval
  in a way the validation misses. The validation catches a
  backward step that happened between the retained and fresh
  creations (the otime order inverts) and one that leaves the
  threshold in the run's future. What it cannot catch: the
  clock steps backwards inside the interval, and a metadata
  change is then stamped with a ctime that lands before T;
  the ctime proof would misread that change as pre-interval.
  That needs a backward clock STEP (an NTP step correction, a
  manual set) during the interval AND a metadata change
  inside the stepped window; NTP slewing, a gradual
  adjustment, cannot produce it. It is accepted, and it is
  the only assumption the proof adds beyond v4's.
- ctime granularity is nanoseconds in the stream and in the
  ioctl on btrfs; the comparison is exact, no rounding.

## What is NOT here

- No tree-search code: v6 is based on the v4 tip and contains
  none of v5's map machinery. The only new kernel interface is
  the single GET_SUBVOL_INFO call per root per run.
- No state format change, no lifecycle change: prepare,
  finalize, rebase and adoption are byte-identical to v4.
- No mount requirement: relatime, noatime and strictatime
  trees all work; the proof's hit rate varies with how much
  atime churn is read-driven (under noatime there is no churn
  and the filter is idle, which is fine, not a requirement).

## Performance expectation

A ctime-proven candidate costs zero retained reads. On the
mass-atime shape (a read sweep over a 200k-file corpus,
atimes stale under relatime), nearly every candidate proves
by ctime, so parse time approaches the stream-decode floor.
Candidates with in-interval ctimes (deliberate utimensat
calls, an application setting times) pay exactly the v4 cost:
one lstat each. Testbed timings are functional and relative
only; the testbed's SSD cannot expose the HDD metadata costs
that motivated this work, and no production timing is claimed
from them.
