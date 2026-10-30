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
//! metadata, link or rename operation) and the mtime carried by
//! its (last) set_times record equals, to the nanosecond, the
//! mtime of the same inode in the retained snapshot. That
//! retained value comes from one bulk TREE_SEARCH_V2 pass over
//! the retained tree (treesearch.rs), keyed by the candidate's
//! fresh inode, with the original per-path retained lstat as
//! the fallback for anything the map does not cover (see
//! prove_retained_mtime and TREESEARCH_DESIGN.md). Atime-only
//! paths are removed from the changed set and counted separately:
//! pure access-time updates (a backup's own reads, an integrity
//! scan walking the tree) are not changes restic needs to hear
//! about. Every doubt fails toward inclusion: a missing or
//! undecodable stream mtime, a path absent from the retained
//! tree, a hardlink sibling reached from a substantive operation,
//! or any non-utime operation on the path keeps it in the changed
//! set. A dropped path keeps its parent-tree atime in the next
//! snapshot; no other tree fact is affected.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::ino;
use crate::stream::{Op, OpKind};
use crate::treesearch;

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
    /// Retained-tree inode -> mtime map from one bulk
    /// TREE_SEARCH_V2 pass (see treesearch.rs), the atime
    /// filter's fast source for the retained side of its proof.
    /// Built lazily on the first candidate that needs a proof;
    /// None when the build failed or has not happened, in which
    /// case every proof takes the per-path stat path, exactly
    /// as before.
    old_mtime_map: Option<HashMap<u64, (i64, i64)>>,
    map_tried: bool,
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
            old_mtime_map: None,
            map_tried: false,
            events: Vec::new(),
        })
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

    /// Build the retained inode -> mtime map on first demand
    /// (one TREE_SEARCH_V2 pass over the retained snapshot; see
    /// treesearch.rs). A failed build is not an error for the
    /// run: the map stays absent, every proof takes the stat
    /// path, and the outcome is a diagnostic event either way,
    /// mirroring the ino-paths-clamped event.
    fn ensure_old_mtime_map(&mut self) {
        if self.map_tried {
            return;
        }
        self.map_tried = true;
        match treesearch::inode_mtimes(&self.old_fd) {
            Ok(map) => {
                self.events.push(Event {
                    kind: "tree-search-map",
                    path: map.len().to_string().into_bytes(),
                });
                self.old_mtime_map = Some(map);
            }
            Err(_) => {
                self.events.push(Event {
                    kind: "tree-search-unavailable",
                    path: Vec::new(),
                });
            }
        }
    }

    /// The atime filter's proof for one candidate: the stream
    /// mtime equals the retained tree's mtime for the same
    /// inode, to the nanosecond.
    ///
    /// Fast source: the candidate's fresh inode (from the
    /// fresh-tree lstat the resolver already performed and
    /// cached for this path) keyed into the tree-search map.
    /// Keying by inode is the same comparison the stat makes:
    /// mtime is a per-inode fact, and a candidate is by
    /// construction a path no substantive op implicated, so
    /// the inode at that path is the retained tree's inode of
    /// the same number (snapshot lineage shares one inode
    /// space; see TREESEARCH_DESIGN.md). Two classes never use
    /// the map: inode 256 (every subvolume root carries it, so
    /// it collides with the retained root's own entry) and map
    /// misses (short items, nested-subvolume trees the search
    /// does not span, any gap at all). Both, and a failed map
    /// build, take the original proof verbatim: an lstat in the
    /// retained tree at the candidate's path.
    fn prove_retained_mtime(&mut self, rel: &[u8], stream_mtime: (i64, u32)) -> bool {
        if let Some(m) = self.meta_new(rel) {
            if m.ino != 256 {
                self.ensure_old_mtime_map();
                if let Some(map) = &self.old_mtime_map {
                    if let Some(&(sec, nsec)) = map.get(&m.ino) {
                        return sec == stream_mtime.0
                            && nsec == stream_mtime.1 as i64;
                    }
                }
            }
        }
        match self.meta_old(rel) {
            Some(m) => {
                m.mtime.0 == stream_mtime.0 && m.mtime.1 == stream_mtime.1 as i64
            }
            None => false,
        }
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
        // utime_cand holds the stream mtime of the last set_times
        // record implicating the name (None = the record carried
        // no decodable mtime); subst holds every name implicated
        // by any other operation. A changed name present in
        // utime_cand and absent from subst is a drop candidate.
        let mut utime_cand: HashMap<Vec<u8>, Option<(i64, u32)>> = HashMap::new();
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
                    utime_cand.insert(f.clone(), op.mtime);
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
        // candidacy (same inode, hence same mtime).
        let mut expanded: HashSet<Vec<u8>> = HashSet::new();
        for p in &changed_inode {
            expanded.insert(p.clone());
            if let Some(m) = self.meta_new(p) {
                if m.typ == Typ::File && m.nlink > 1 {
                    let cand_mtime = utime_cand.get(p).copied();
                    for name in self.fresh_names_of_inode(m.ino)? {
                        if subst.contains(p) {
                            subst.insert(name.clone());
                        } else if let Some(mt) = cand_mtime {
                            utime_cand.entry(name.clone()).or_insert(mt);
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
        // proof: the stream mtime exists and equals the retained
        // tree's mtime for the same inode to the nanosecond (see
        // prove_retained_mtime for where that value comes from).
        // Every other outcome (no stream mtime, no retained path,
        // mtimes apart) keeps the name in the changed set.
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
            for p in candidates {
                let stream_mtime = utime_cand.get(&p).copied().flatten();
                let proven = match stream_mtime {
                    Some(mt) => self.prove_retained_mtime(&p, mt),
                    None => false,
                };
                if proven {
                    changed.remove(&p);
                    atime_only += 1;
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
        }
    }

    fn resolve(old: &Path, new: &Path, ops: &[Op]) -> (HashSet<Vec<u8>>, u64) {
        let mut r = Resolver::new(old.to_path_buf(), new.to_path_buf()).unwrap();
        let (changed, _deleted, atime_only) = r.resolve(ops).unwrap();
        (changed, atime_only)
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

    fn resolve_with_map(
        old: &Path,
        new: &Path,
        ops: &[Op],
        map: HashMap<u64, (i64, i64)>,
    ) -> (HashSet<Vec<u8>>, u64) {
        let mut r = Resolver::new(old.to_path_buf(), new.to_path_buf()).unwrap();
        r.map_tried = true;
        r.old_mtime_map = Some(map);
        let (changed, _deleted, atime_only) = r.resolve(ops).unwrap();
        (changed, atime_only)
    }

    #[test]
    fn map_hit_equal_drops_on_map_authority() {
        // The map is the proof's source when it covers the
        // inode: here it deliberately disagrees with what a
        // retained lstat would say (the retained file's mtime
        // is the put() constant, the stream mtime is a second
        // later), and the map, keyed by the fresh inode, carries
        // the stream value. The drop follows the map.
        let (old, new) = temp_tree("mapdrop");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let ino = fs::symlink_metadata(new.join("f.bin")).unwrap().ino();
        let (sec, nsec) = mtime_of(&old, "f.bin");
        let stream_mt = (sec + 1, nsec);
        let mut map = HashMap::new();
        map.insert(ino, (stream_mt.0, stream_mt.1 as i64));
        let (changed, atime_only) = resolve_with_map(
            &old,
            &new,
            &[op(1, OpKind::Utime, "f.bin", Some(stream_mt))],
            map,
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
    }

    #[test]
    fn map_hit_unequal_keeps_even_when_stat_would_drop() {
        // The negative verdict is the map's too: the stream
        // mtime equals the retained lstat's, but the map entry
        // for the inode says the retained mtime is a second
        // later, so the path stays changed.
        let (old, new) = temp_tree("mapkeep");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let ino = fs::symlink_metadata(new.join("f.bin")).unwrap().ino();
        let mt = mtime_of(&old, "f.bin");
        let mut map = HashMap::new();
        map.insert(ino, (mt.0 + 1, mt.1 as i64));
        let (changed, atime_only) = resolve_with_map(
            &old,
            &new,
            &[op(1, OpKind::Utime, "f.bin", Some(mt))],
            map,
        );
        assert!(changed.contains(b"f.bin".as_slice()));
        assert_eq!(atime_only, 0);
    }

    #[test]
    fn map_miss_falls_back_to_stat_proof() {
        // An empty (but successfully built) map proves nothing;
        // the original stat proof decides, and it drops here.
        let (old, new) = temp_tree("mapmiss");
        put(&old, "f.bin");
        put(&new, "f.bin");
        let mt = mtime_of(&old, "f.bin");
        let (changed, atime_only) = resolve_with_map(
            &old,
            &new,
            &[op(1, OpKind::Utime, "f.bin", Some(mt))],
            HashMap::new(),
        );
        assert!(changed.is_empty(), "changed: {changed:?}");
        assert_eq!(atime_only, 1);
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
