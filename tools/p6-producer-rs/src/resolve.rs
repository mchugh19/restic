//! Turn a classified send-stream operation list into the final
//! changed/deleted path lists.
//!
//! This is a faithful port of the resolution half of sendparse.py,
//! the Phase A reference parser, whose verdicts stand (mutation
//! matrix 25/25, 40 randomised batches exact, both corpus shapes
//! exact). The algorithm is unchanged, including its quirks; what
//! changes is where tree facts come from. The reference consulted
//! two full metadata walks of the snapshots. This resolver asks
//! the filesystem only about paths the stream actually named:
//! lstat for membership and type, a recursive listing only of
//! subtrees that a directory move or deletion implicates, and
//! BTRFS_IOC_INO_PATHS for the fresh names of a changed inode.
//! The one exception to "only what the stream named": an inode
//! whose names do not fit in the kernel's one-page INO_PATHS
//! container is resolved by a single metadata walk of the fresh
//! tree (built once, then cached), because the ioctl cannot
//! return a complete list for it at any buffer size.
//!
//! Semantics, as in the reference:
//!   * Names in the stream are valid at their point in the stream,
//!     so every path is resolved through the rename sequence:
//!     `final_name` applies later renames forward (giving the
//!     path's spelling in the fresh tree), `retained_name` applies
//!     earlier renames in reverse (its spelling in the retained
//!     tree).
//!   * An inode change (content, metadata, link count) applies to
//!     every fresh name of the inode; a positional change (its
//!     parent directory moved) does not. The two sets are kept
//!     separate until the end for exactly this reason.
//!   * A moved directory contributes its whole subtree (old paths
//!     deleted, corresponding new paths changed-positionally); a
//!     deleted directory likewise contributes its subtree as
//!     deletions.
//!   * Final sets are intersected with tree reality: changed holds
//!     only fresh paths, deleted only retained paths that are
//!     absent from the fresh tree and not also changed.
//!
//! One deliberate divergence from the reference: the atime-only
//! filter. A path is atime-only when every stream operation that
//! implicates it is a set_times record (no create, content,
//! metadata, link or rename operation) and the change it reports
//! is proved to be atime movement only. Atime-only paths are
//! removed from the changed set and counted separately: pure
//! access-time updates (a backup's own reads, an integrity scan
//! walking the tree) are not changes restic needs to hear about.
//! A dropped path keeps its parent-tree atime in the next
//! snapshot; no other tree fact is affected.
//!
//! Two proofs can establish a candidate, tried in this order
//! (see CTIME_DESIGN.md for the full argument):
//!   1. The ctime proof (v6, no retained-side lookup at all):
//!      read-driven atime updates never move an inode's ctime,
//!      and every metadata change stamps ctime with the moment
//!      of the change, so a candidate whose set_times ctime is
//!      strictly older than the retained snapshot's creation
//!      time cannot have changed inside the interval; the op
//!      can only be reporting atime movement. The threshold is
//!      the retained snapshot's own kernel otime, read with its
//!      uuid via BTRFS_IOC_GET_SUBVOL_INFO (see subvol.rs), and
//!      the proof is disabled for the whole run whenever that
//!      threshold is missing or suspect.
//!   2. The stat proof (the v3/v4 proof, verbatim): the mtime
//!      carried by the candidate's (last) set_times record
//!      equals, to the nanosecond, the mtime of the same path
//!      in the retained snapshot (one lstat).
//! Every doubt fails toward inclusion: a missing or undecodable
//! stream ctime or mtime, a ctime inside the interval, an
//! unavailable threshold, a path absent from the retained tree,
//! a hardlink sibling reached from a substantive operation, or
//! any non-utime operation on the path keeps it in the changed
//! set (or routes it to the stat proof, whose own doubts all
//! keep it).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ino;
use crate::stream::{Op, OpKind};
use crate::subvol;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Typ {
    File,
    Dir,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    typ: Typ,
    ino: u64,
    nlink: u64,
    /// lstat mtime as (seconds, nanoseconds); the atime filter
    /// compares the retained tree's value against the mtime a
    /// set_times record carries.
    mtime: (i64, i64),
}

#[derive(Clone, Debug)]
pub struct Event {
    pub kind: &'static str,
    /// The stream-time path the event concerns (diagnostics; the
    /// gates consume kinds only).
    #[allow(dead_code)]
    pub path: Vec<u8>,
}

fn is_temp(path: &[u8]) -> bool {
    // Basename matches ^o\d+-\d+-\d+$ (kernel staging names).
    let base = match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    if base.len() < 6 || base[0] != b'o' {
        return false;
    }
    let parts: Vec<&[u8]> = base[1..].split(|&b| b == b'-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.iter().all(|b| b.is_ascii_digit()))
}

fn has_dir_prefix(p: &[u8], prefix: &[u8]) -> bool {
    p.len() > prefix.len() && p.starts_with(prefix) && p[prefix.len()] == b'/'
}

type RenameSeq = Vec<(u64, Vec<u8>, Vec<u8>)>; // (seq, src, dst)

fn final_name(renames: &RenameSeq, path: &[u8], seq: u64) -> Vec<u8> {
    let mut p = path.to_vec();
    for (s, src, dst) in renames {
        if *s <= seq {
            continue;
        }
        if p == *src {
            p = dst.clone();
        } else if !src.is_empty() && has_dir_prefix(&p, src) {
            let mut np = dst.clone();
            np.extend_from_slice(&p[src.len()..]);
            p = np;
        }
    }
    p
}

fn retained_name(renames: &RenameSeq, path: &[u8], seq: u64) -> Vec<u8> {
    let mut p = path.to_vec();
    for (s, src, dst) in renames.iter().rev() {
        if *s >= seq {
            continue;
        }
        if p == *dst {
            p = src.clone();
        } else if !dst.is_empty() && has_dir_prefix(&p, dst) {
            let mut np = src.clone();
            np.extend_from_slice(&p[dst.len()..]);
            p = np;
        }
    }
    p
}

pub struct Resolver {
    old_root: PathBuf,
    new_root: PathBuf,
    old_fd: File,
    new_fd: File,
    old_meta: HashMap<Vec<u8>, Option<Meta>>,
    new_meta: HashMap<Vec<u8>, Option<Meta>>,
    /// Fresh-tree inode -> names map, built by the one walk a
    /// page-clamped inode forces (see fresh_names_by_walk); None
    /// until the first clamped inode appears.
    new_by_ino: Option<HashMap<u64, Vec<Vec<u8>>>>,
    /// The ctime proof's threshold: the retained snapshot's
    /// creation time as (seconds, nanoseconds). None = the proof
    /// is disabled for this run (threshold unavailable or
    /// suspect, or pinned off); every candidate then takes the
    /// stat proof, exactly as in v4. Unset until first computed
    /// (see ctime_threshold); the outer Option is the cache.
    ctime_threshold: Option<Option<(i64, u32)>>,
    /// Candidates dropped by the ctime proof in this resolution
    /// (diagnostics; reported as one aggregate event).
    ctime_proven: u64,
    pub events: Vec<Event>,
}

impl Resolver {
    pub fn new(old_root: PathBuf, new_root: PathBuf) -> io::Result<Self> {
        let old_fd = File::open(&old_root)?;
        let new_fd = File::open(&new_root)?;
        Ok(Resolver {
            old_root,
            new_root,
            old_fd,
            new_fd,
            old_meta: HashMap::new(),
            new_meta: HashMap::new(),
            new_by_ino: None,
            ctime_threshold: None,
            ctime_proven: 0,
            events: Vec::new(),
        })
    }

    /// Pin the ctime threshold instead of deriving it from the
    /// retained snapshot (unit tests, which resolve against
    /// plain temp directories, use this; the lifecycle never
    /// does). Pinning None disables the ctime proof outright.
    pub fn set_ctime_threshold(&mut self, threshold: Option<(i64, u32)>) {
        self.ctime_threshold = Some(threshold);
    }

    /// The retained snapshot's creation time, the ctime proof's
    /// threshold, computed once and cached. The value comes from
    /// the kernel: BTRFS_IOC_GET_SUBVOL_INFO on the retained
    /// root returns the snapshot's uuid and otime together, so
    /// the timestamp belongs to exactly the snapshot being
    /// diffed, whatever lifecycle path produced it.
    ///
    /// The proof is disabled (None) whenever anything is off:
    /// the drill hook P6_NO_CTIME_PROOF is set; either root is
    /// not a btrfs subvolume the ioctl can describe; the two
    /// snapshots share a uuid (not a retained/fresh pair); the
    /// retained otime is not strictly before the fresh otime
    /// (a wall clock that went backwards between the two
    /// creations, or a clock too coarse to order them: either
    /// way the interval's clock cannot be trusted); the
    /// retained otime is in the future relative to this run's
    /// clock; or the run's clock cannot be read at all. A
    /// disabled proof costs one ioctl pair per run and changes
    /// nothing else: every candidate takes the stat proof.
    fn ctime_threshold(&mut self) -> Option<(i64, u32)> {
        if let Some(t) = self.ctime_threshold {
            return t;
        }
        let derived = self.derive_ctime_threshold();
        self.ctime_threshold = Some(derived);
        derived
    }

    fn derive_ctime_threshold(&self) -> Option<(i64, u32)> {
        if std::env::var_os("P6_NO_CTIME_PROOF").is_some() {
            return None;
        }
        let old = subvol::subvol_info(&self.old_fd).ok()?;
        let new = subvol::subvol_info(&self.new_fd).ok()?;
        if old.uuid == new.uuid {
            return None;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        let now = (now.as_secs() as i64, now.subsec_nanos());
        let strictly_before = |a: (i64, u32), b: (i64, u32)| {
            a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
        };
        if !strictly_before(old.otime, new.otime) {
            return None;
        }
        if !strictly_before(old.otime, now) && old.otime != now {
            return None;
        }
        Some(old.otime)
    }

    fn lstat(root: &Path, rel: &[u8]) -> Option<Meta> {
        let full = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(Path::new(std::ffi::OsStr::from_bytes(rel)))
        };
        let md = fs::symlink_metadata(full).ok()?;
        let ft = md.file_type();
        let typ = if ft.is_dir() {
            Typ::Dir
        } else if ft.is_file() {
            Typ::File
        } else {
            Typ::Other
        };
        Some(Meta {
            typ,
            ino: md.ino(),
            nlink: md.nlink(),
            mtime: (md.mtime(), md.mtime_nsec()),
        })
    }

    fn meta_old(&mut self, rel: &[u8]) -> Option<Meta> {
        if let Some(m) = self.old_meta.get(rel) {
            return *m;
        }
        let m = Self::lstat(&self.old_root, rel);
        self.old_meta.insert(rel.to_vec(), m);
        m
    }

    fn meta_new(&mut self, rel: &[u8]) -> Option<Meta> {
        if let Some(m) = self.new_meta.get(rel) {
            return *m;
        }
        let m = Self::lstat(&self.new_root, rel);
        self.new_meta.insert(rel.to_vec(), m);
        m
    }

    fn in_old(&mut self, rel: &[u8]) -> bool {
        self.meta_old(rel).is_some()
    }

    fn in_new(&mut self, rel: &[u8]) -> bool {
        self.meta_new(rel).is_some()
    }

    fn fresh_names_of_inode(&mut self, ino: u64) -> io::Result<Vec<Vec<u8>>> {
        match ino::ino_paths(&self.new_fd, ino)? {
            ino::InoResolution::Complete(paths) => Ok(paths),
            ino::InoResolution::Clamped => {
                self.events.push(Event {
                    kind: "ino-paths-clamped",
                    path: ino.to_string().into_bytes(),
                });
                Ok(self.fresh_names_by_walk(ino))
            }
        }
    }

    /// Every fresh name of `ino`, found by walking the fresh tree.
    /// Used only for inodes whose path list the kernel's one-page
    /// INO_PATHS container cannot hold. The walk runs at most once
    /// per resolution; its inode -> names map is cached, so
    /// further clamped inodes are free. It mirrors subtree_old's
    /// treatment of nested subvolumes (a subvolume root, inode
    /// 256, is recorded as a leaf and not descended into: its
    /// contents belong to that subvolume, not this one) and feeds
    /// the new_meta cache the way subtree_old feeds old_meta.
    fn fresh_names_by_walk(&mut self, ino: u64) -> Vec<Vec<u8>> {
        if self.new_by_ino.is_none() {
            let mut by_ino: HashMap<u64, Vec<Vec<u8>>> = HashMap::new();
            let mut stack: Vec<Vec<u8>> = vec![Vec::new()];
            while let Some(d) = stack.pop() {
                let full = if d.is_empty() {
                    self.new_root.clone()
                } else {
                    self.new_root
                        .join(Path::new(std::ffi::OsStr::from_bytes(&d)))
                };
                let entries = match fs::read_dir(&full) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let mut rel = d.clone();
                    if !rel.is_empty() {
                        rel.push(b'/');
                    }
                    rel.extend_from_slice(name.as_bytes());
                    if let Ok(md) = entry.metadata() {
                        let ft = md.file_type();
                        let typ = if ft.is_dir() {
                            Typ::Dir
                        } else if ft.is_file() {
                            Typ::File
                        } else {
                            Typ::Other
                        };
                        self.new_meta.insert(
                            rel.clone(),
                            Some(Meta {
                                typ,
                                ino: md.ino(),
                                nlink: md.nlink(),
                                mtime: (md.mtime(), md.mtime_nsec()),
                            }),
                        );
                        if ft.is_file() {
                            by_ino.entry(md.ino()).or_default().push(rel.clone());
                        }
                        if ft.is_dir() && md.ino() != 256 {
                            stack.push(rel);
                        }
                    }
                }
            }
            self.new_by_ino = Some(by_ino);
        }
        self.new_by_ino
            .as_ref()
            .unwrap()
            .get(&ino)
            .cloned()
            .unwrap_or_default()
    }

    /// All descendants of `dir` in the retained tree (relative
    /// paths, dir itself excluded), mirroring the reference's
    /// record set: a nested subvolume root (inode 256) is recorded
    /// as a leaf and not descended into.
    fn subtree_old(&mut self, dir: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut stack: Vec<Vec<u8>> = vec![dir.to_vec()];
        while let Some(d) = stack.pop() {
            let full = if d.is_empty() {
                self.old_root.clone()
            } else {
                self.old_root
                    .join(Path::new(std::ffi::OsStr::from_bytes(&d)))
            };
            let entries = match fs::read_dir(&full) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let mut rel = d.clone();
                if !rel.is_empty() {
                    rel.push(b'/');
                }
                rel.extend_from_slice(name.as_bytes());
                let is_dir_leaf = match entry.metadata() {
                    Ok(md) => {
                        let ft = md.file_type();
                        let typ = if ft.is_dir() {
                            Typ::Dir
                        } else if ft.is_file() {
                            Typ::File
                        } else {
                            Typ::Other
                        };
                        self.old_meta.insert(
                            rel.clone(),
                            Some(Meta {
                                typ,
                                ino: md.ino(),
                                nlink: md.nlink(),
                                mtime: (md.mtime(), md.mtime_nsec()),
                            }),
                        );
                        ft.is_dir() && md.ino() != 256
                    }
                    Err(_) => false,
                };
                out.push(rel.clone());
                if is_dir_leaf {
                    stack.push(rel);
                }
            }
        }
        out
    }

    pub fn resolve(
        &mut self,
        ops: &[Op],
    ) -> io::Result<(HashSet<Vec<u8>>, HashSet<Vec<u8>>, u64)> {
        let renames: RenameSeq = ops
            .iter()
            .filter(|o| o.kind == OpKind::Rename)
            .map(|o| (o.seq, o.path.clone(), o.aux.clone().unwrap_or_default()))
            .collect();

        let mut changed_inode: HashSet<Vec<u8>> = HashSet::new();
        let mut changed_positional: HashSet<Vec<u8>> = HashSet::new();
        let mut deleted: HashSet<Vec<u8>> = HashSet::new();
        let mut moves: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        // Atime-filter bookkeeping, keyed by final (fresh) name:
        // utime_cand holds the (mtime, ctime) pair of the last
        // set_times record implicating the name (each None = the
        // record carried no decodable attribute of that kind);
        // subst holds every name implicated by any other
        // operation. A changed name present in utime_cand and
        // absent from subst is a drop candidate.
        let mut utime_cand: HashMap<
            Vec<u8>,
            (Option<(i64, u32)>, Option<(i64, u32)>),
        > = HashMap::new();
        let mut subst: HashSet<Vec<u8>> = HashSet::new();

        for op in ops {
            let path = &op.path;
            match op.kind {
                OpKind::Create | OpKind::Modify => {
                    let f = final_name(&renames, path, op.seq);
                    subst.insert(f.clone());
                    if self.in_new(&f) {
                        changed_inode.insert(f);
                    } else if !is_temp(&f) && !is_temp(path) {
                        self.events.push(Event {
                            kind: "op-path-unresolved",
                            path: path.clone(),
                        });
                    }
                }
                OpKind::Utime => {
                    // Resolves exactly like a Modify; the only
                    // difference is the candidacy record (and the
                    // absence of a substantive mark).
                    let f = final_name(&renames, path, op.seq);
                    utime_cand.insert(f.clone(), (op.mtime, op.ctime));
                    if self.in_new(&f) {
                        changed_inode.insert(f);
                    } else if !is_temp(&f) && !is_temp(path) {
                        self.events.push(Event {
                            kind: "op-path-unresolved",
                            path: path.clone(),
                        });
                    }
                }
                OpKind::Link => {
                    let f = final_name(&renames, path, op.seq);
                    subst.insert(f.clone());
                    if self.in_new(&f) {
                        changed_inode.insert(f);
                    } else if !is_temp(&f) {
                        self.events.push(Event {
                            kind: "op-path-unresolved",
                            path: path.clone(),
                        });
                    }
                }
                OpKind::Delete => {
                    let o = retained_name(&renames, path, op.seq);
                    let f = final_name(&renames, path, op.seq);
                    if self.in_old(&o) && !self.in_new(&f) {
                        deleted.insert(o.clone());
                    }
                    // Removing one name of a multiply-linked inode
                    // changes the inode (nlink, ctime): its
                    // surviving fresh names are inode-changed even
                    // though the stream names only the removed one.
                    if let Some(m) = self.meta_old(&o) {
                        if m.typ == Typ::File {
                            for name in self.fresh_names_of_inode(m.ino)? {
                                subst.insert(name.clone());
                                changed_inode.insert(name);
                            }
                        }
                    }
                }
                OpKind::Rename => {
                    let aux = op.aux.clone().unwrap_or_default();
                    let o = retained_name(&renames, path, op.seq);
                    let f = final_name(&renames, &aux, op.seq);
                    if self.in_old(&o) {
                        if !self.in_new(&f) {
                            deleted.insert(o);
                        } else if f != o {
                            deleted.insert(o.clone());
                            changed_positional.insert(f.clone());
                            if let Some(m) = self.meta_old(&o) {
                                if m.typ == Typ::Dir {
                                    moves.push((o, f));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Hardlink sibling expansion over inode-changed paths only:
        // an inode change applies to every name of the inode, but a
        // positional change of one name says nothing about the
        // inode's other names elsewhere in the tree. The atime
        // bookkeeping follows the same propagation: a name reached
        // from a substantive operation is substantive; a name
        // reached only from a utime candidate inherits the
        // candidacy (same inode, hence the same mtime and ctime:
        // both proofs are per-inode facts).
        let mut expanded: HashSet<Vec<u8>> = HashSet::new();
        for p in &changed_inode {
            expanded.insert(p.clone());
            if let Some(m) = self.meta_new(p) {
                if m.typ == Typ::File && m.nlink > 1 {
                    let cand_times = utime_cand.get(p).copied();
                    for name in self.fresh_names_of_inode(m.ino)? {
                        if subst.contains(p) {
                            subst.insert(name.clone());
                        } else if let Some(times) = cand_times {
                            utime_cand.entry(name.clone()).or_insert(times);
                        }
                        expanded.insert(name);
                    }
                }
            }
        }
        let mut changed_inode: HashSet<Vec<u8>> = HashSet::new();
        for p in expanded {
            if self.in_new(&p) {
                changed_inode.insert(p);
            }
        }

        // Subtree effects of directory moves and deletions
        // (positional only: no sibling expansion from here). As in
        // the reference, the membership guard uses the changed set
        // as it stood before the moves were applied.
        let changed_pre: HashSet<Vec<u8>> =
            changed_inode.union(&changed_positional).cloned().collect();
        for (o, f) in &moves {
            for c in self.subtree_old(o) {
                if !self.in_new(&c) && !changed_pre.contains(&c) {
                    deleted.insert(c.clone());
                }
                let mut mapped = f.clone();
                mapped.extend_from_slice(&c[o.len()..]);
                if self.in_new(&mapped) {
                    changed_positional.insert(mapped);
                }
            }
        }
        let deleted_snapshot: Vec<Vec<u8>> = deleted.iter().cloned().collect();
        for d in &deleted_snapshot {
            let is_dir = matches!(self.meta_old(d), Some(m) if m.typ == Typ::Dir);
            if is_dir {
                for c in self.subtree_old(d) {
                    if !self.in_new(&c) && !changed_pre.contains(&c) {
                        deleted.insert(c);
                    }
                }
            }
        }

        let mut changed: HashSet<Vec<u8>> =
            changed_inode.union(&changed_positional).cloned().collect();

        // The atime-only filter. A candidate is a changed name
        // whose implicating operations were all set_times records
        // (present in utime_cand, absent from subst) and which is
        // not positionally changed. It is dropped only on positive
        // proof, tried in order:
        //   1. ctime proof: the record's ctime exists and is
        //      strictly older than the retained snapshot's
        //      creation time (the threshold). No metadata change
        //      can have happened inside the interval (it would
        //      have stamped ctime inside it), and read-driven
        //      atime updates never move ctime, so the op can
        //      only be reporting atime movement. No retained
        //      lookup happens for such a candidate.
        //   2. stat proof (v3/v4, verbatim): the stream mtime
        //      exists and equals the retained tree's mtime for
        //      the same path to the nanosecond.
        // Equality of ctime and threshold is ambiguous and falls
        // through, as do a missing ctime, a disabled threshold
        // and every stat-proof doubt: the name stays in the
        // changed set unless a proof is positive.
        let mut atime_only: u64 = 0;
        if !utime_cand.is_empty() {
            let candidates: Vec<Vec<u8>> = changed
                .iter()
                .filter(|p| {
                    utime_cand.contains_key(*p)
                        && !subst.contains(*p)
                        && !changed_positional.contains(*p)
                })
                .cloned()
                .collect();
            let had_candidates = !candidates.is_empty();
            for p in candidates {
                let (stream_mtime, stream_ctime) = utime_cand
                    .get(&p)
                    .copied()
                    .unwrap_or((None, None));
                let mut proven = false;
                if let (Some(ct), Some(thr)) =
                    (stream_ctime, self.ctime_threshold())
                {
                    if ct.0 < thr.0 || (ct.0 == thr.0 && ct.1 < thr.1) {
                        proven = true;
                        self.ctime_proven += 1;
                    }
                }
                if !proven {
                    proven = match (stream_mtime, self.meta_old(&p)) {
                        (Some((sec, nsec)), Some(m)) => {
                            m.mtime.0 == sec && m.mtime.1 == nsec as i64
                        }
                        _ => false,
                    };
                }
                if proven {
                    changed.remove(&p);
                    atime_only += 1;
                }
            }
            if had_candidates {
                // One aggregate diagnostic per resolution, in the
                // style of the other events: how many drops the
                // ctime proof carried, or that it was disabled and
                // every proof was a stat.
                if self.ctime_threshold().is_some() {
                    self.events.push(Event {
                        kind: "ctime-proof",
                        path: self.ctime_proven.to_string().into_bytes(),
                    });
                } else {
                    self.events.push(Event {
                        kind: "ctime-proof-disabled",
                        path: Vec::new(),
                    });
                }
            }
        }

        let mut final_deleted: HashSet<Vec<u8>> = HashSet::new();
        for p in &deleted {
            if self.in_old(p) && !self.in_new(p) && !changed.contains(p) {
                final_deleted.insert(p.clone());
            }
        }
        Ok((changed, final_deleted, atime_only))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn ren(seq: u64, src: &str, dst: &str) -> (u64, Vec<u8>, Vec<u8>) {
        (seq, src.as_bytes().to_vec(), dst.as_bytes().to_vec())
    }

    // -- the atime-only filter ------------------------------------
    // The resolver only ever lstats these trees (no btrfs needed):
    // retained/ and fresh/ are plain temp directories, and the ops
    // are the classified records a real stream would have carried.
    fn temp_tree(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "p6-resolve-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let old = base.join("retained");
        let new = base.join("fresh");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        (old, new)
    }

    fn put(root: &Path, rel: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, b"x").unwrap();
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789))
            .unwrap();
    }

    fn mtime_of(root: &Path, rel: &str) -> (i64, u32) {
        let md = fs::symlink_metadata(root.join(rel)).unwrap();
        (md.mtime(), md.mtime_nsec() as u32)
    }

    fn op(seq: u64, kind: OpKind, path: &str, mtime: Option<(i64, u32)>) -> Op {
        Op {
            seq,
            kind,
            path: path.as_bytes().to_vec(),
            aux: None,
            mtime,
            ctime: None,
        }
    }

    fn op_ctime(
        seq: u64,
        path: &str,
        mtime: Option<(i64, u32)>,
        ctime: Option<(i64, u32)>,
    ) -> Op {
        Op {
            seq,
            kind: OpKind::Utime,
            path: path.as_bytes().to_vec(),
            aux: None,
            mtime,
            ctime,
        }
    }

    fn resolve(old: &Path, new: &Path, ops: &[Op]) -> (HashSet<Vec<u8>>, u64) {
        let mut r = Resolver::new(old.to_path_buf(), new.to_path_buf()).unwrap();
        let (changed, _deleted, atime_only) = r.resolve(ops).unwrap();
        (changed, atime_only)
    }

    /// Resolve with the ctime threshold pinned (the temp trees
    /// are not btrfs, so auto-derivation would disable the
    /// proof). Returns the changed set, the atime_only count and
    /// the resolver's events, whose ctime-proof aggregate carries
    /// its drop count in the path field.
    fn resolve_pinned(
        old: &Path,
        new: &Path,
        ops: &[Op],
        threshold: Option<(i64, u32)>,
    ) -> (HashSet<Vec<u8>>, u64, Vec<Event>) {
        let mut r = Resolver::new(old.to_path_buf(), new.to_path_buf()).unwrap();
        r.set_ctime_threshold(threshold);
        let (changed, _deleted, atime_only) = r.resolve(ops).unwrap();
        (changed, atime_only, r.events)
    }

    fn ctime_event(events: &[Event], kind: &str) -> Option<String> {
        events
            .iter()
            .find(|e| e.kind == kind)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
    }

    // The pinned threshold for the ctime tests: the fixture
    // files' fixed mtime is 1_700_000_000.123456789 (see put()).
    const THR: (i64, u32) = (1_700_000_100, 0);
    const CT_OLD: (i64, u32) = (1_700_000_050, 0); // before THR
    const CT_IN: (i64, u32) = (1_700_000_150, 0); // inside interval

    #[test]
    fn ctime_proof_drops_when_ctime_predates_retained() {
        let (old, new) = temp_tree("ctimedrop");
        put(&old, "f.bin");
        put(&new, "f.bin");
        // The stream mtime deliberately disagrees with the
        // retained tree: the stat proof alone would keep this
        // path. The ctime proof is what drops it (its content:
        // no metadata change can have happened inside the
        // interval, so a real stream of this shape cannot carry
        // a moved mtime; the combination is the proof's whole
        // point, and the clock assumptions are CTIME_DESIGN.md's).
        let (sec, nsec) = mtime_of(&old, "f.bin");
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some((sec + 5, nsec)), Some(CT_OLD))],
            Some(THR),
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
        assert_eq!(
            ctime_event(&events, "ctime-proof").as_deref(),
            Some("1")
        );
    }

    #[test]
    fn ctime_equal_to_threshold_falls_through_to_stat() {
        let (old, new) = temp_tree("ctimeequal");
        put(&old, "f.bin");
        put(&new, "f.bin");
        // ctime == threshold is ambiguous (a change stamped in
        // the retained snapshot's own creation instant): the
        // ctime proof must not fire; the stat proof drops the
        // path because the mtime did not move.
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some(mt), Some(THR))],
            Some(THR),
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
        assert_eq!(
            ctime_event(&events, "ctime-proof").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn ctime_inside_interval_stat_governs_both_ways() {
        // (a) mtime moved: kept, by the stat proof.
        let (old, new) = temp_tree("ctimeinkept");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let (sec, nsec) = mtime_of(&old, "f.bin");
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some((sec + 5, nsec)), Some(CT_IN))],
            Some(THR),
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
        assert_eq!(
            ctime_event(&events, "ctime-proof").as_deref(),
            Some("0")
        );
        // (b) mtime unmoved (the deliberate utimensat atime set):
        // dropped, by the stat proof, not the ctime proof.
        let (old, new) = temp_tree("ctimeindrop");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some(mt), Some(CT_IN))],
            Some(THR),
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
        assert_eq!(
            ctime_event(&events, "ctime-proof").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn missing_ctime_falls_through_to_stat() {
        let (old, new) = temp_tree("ctimemissing");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let mt = mtime_of(&old, "f.bin");
        // No ctime on the record: threshold or not, the verdict
        // is the stat proof's (drop here, mtime unmoved).
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some(mt), None)],
            Some(THR),
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
        assert_eq!(
            ctime_event(&events, "ctime-proof").as_deref(),
            Some("0")
        );
        // And with the mtime moved, the same shape is kept.
        let (changed, atime_only, _) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some((mt.0 + 5, mt.1)), None)],
            Some(THR),
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn disabled_threshold_falls_through_to_stat() {
        let (old, new) = temp_tree("ctimedisabled");
        put(&old, "f.bin");
        put(&new, "f.bin");
        // Threshold pinned off: even a pre-retained ctime proves
        // nothing, so the moved mtime keeps the path...
        let (sec, nsec) = mtime_of(&old, "f.bin");
        let (changed, atime_only, events) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some((sec + 5, nsec)), Some(CT_OLD))],
            None,
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
        assert!(ctime_event(&events, "ctime-proof-disabled").is_some());
        assert!(ctime_event(&events, "ctime-proof").is_none());
        // ...and the unmoved mtime still drops it, by stat.
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only, _) = resolve_pinned(
            &old,
            &new,
            &[op_ctime(1, "f.bin", Some(mt), Some(CT_OLD))],
            None,
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
    }

    #[test]
    fn utime_only_dropped_when_mtime_unmoved() {
        let (old, new) = temp_tree("drop");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only) =
            resolve(&old, &new, &[op(1, OpKind::Utime, "f.bin", Some(mt))]);
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
    }

    #[test]
    fn utime_kept_when_mtime_moved() {
        let (old, new) = temp_tree("moved");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let (sec, nsec) = mtime_of(&old, "f.bin");
        let (changed, atime_only) = resolve(
            &old,
            &new,
            &[op(1, OpKind::Utime, "f.bin", Some((sec + 5, nsec)))],
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn utime_kept_without_stream_mtime() {
        let (old, new) = temp_tree("nostream");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let (changed, atime_only) =
            resolve(&old, &new, &[op(1, OpKind::Utime, "f.bin", None)]);
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn utime_kept_when_absent_from_retained() {
        let (old, new) = temp_tree("newfile");
        put(&new, "new.bin");
        let (changed, atime_only) = resolve(
            &old,
            &new,
            &[op(
                1,
                OpKind::Utime,
                "new.bin",
                Some((1_700_000_000, 123_456_789)),
            )],
        );
        assert!(changed.contains(b"new.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn utime_plus_write_is_kept() {
        let (old, new) = temp_tree("pluswrite");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only) = resolve(
            &old,
            &new,
            &[
                op(1, OpKind::Utime, "f.bin", Some(mt)),
                op(2, OpKind::Modify, "f.bin", None),
            ],
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn walk_finds_every_hardlink_name() {
        let (old, new) = temp_tree("walk");
        put(&new, "a/f.bin");
        let src = new.join("a/f.bin");
        fs::create_dir_all(new.join("b")).unwrap();
        fs::create_dir_all(new.join("c/deep")).unwrap();
        fs::hard_link(&src, new.join("b/f-link.bin")).unwrap();
        fs::hard_link(&src, new.join("c/deep/f-link2.bin")).unwrap();
        put(&new, "solo.bin");
        let ino = fs::symlink_metadata(&src).unwrap().ino();
        let mut r = Resolver::new(old, new).unwrap();
        let mut names = r.fresh_names_by_walk(ino);
        names.sort();
        assert_eq!(
            names,
            vec![
                b"a/f.bin".to_vec(),
                b"b/f-link.bin".to_vec(),
                b"c/deep/f-link2.bin".to_vec()
            ]
        );
        // An inode the tree does not contain resolves to no names.
        assert!(r.fresh_names_by_walk(ino + 999_999).is_empty());
    }

    #[test]
    fn temp_names() {
        assert!(is_temp(b"o263-205-0"));
        assert!(is_temp(b"dir/o263-205-0"));
        assert!(!is_temp(b"o263-205"));
        assert!(!is_temp(b"file.txt"));
        assert!(!is_temp(b"o-1-2"));
        assert!(!is_temp(b""));
    }

    #[test]
    fn final_and_retained_names() {
        let renames = vec![ren(2, "a", "b"), ren(5, "b", "c")];
        // A path spelled before both renames ends at the last name.
        assert_eq!(final_name(&renames, b"a/x", 1), b"c/x");
        // A path spelled after the first rename only follows the second.
        assert_eq!(final_name(&renames, b"b/x", 3), b"c/x");
        // Reverse resolution walks the sequence backwards.
        assert_eq!(retained_name(&renames, b"c/x", 9), b"a/x");
        // Spelled between the two renames, only the first is undone.
        assert_eq!(retained_name(&renames, b"b/x", 4), b"a/x");
        // Prefix rewriting is component-aware: "ab" is not under "a".
        assert_eq!(final_name(&renames, b"ab/x", 1), b"ab/x");
    }
}
