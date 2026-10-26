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

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::ino;
use crate::stream::{Op, OpKind};

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
    new_fd: File,
    old_meta: HashMap<Vec<u8>, Option<Meta>>,
    new_meta: HashMap<Vec<u8>, Option<Meta>>,
    pub events: Vec<Event>,
}

impl Resolver {
    pub fn new(old_root: PathBuf, new_root: PathBuf) -> io::Result<Self> {
        let new_fd = File::open(&new_root)?;
        Ok(Resolver {
            old_root,
            new_root,
            new_fd,
            old_meta: HashMap::new(),
            new_meta: HashMap::new(),
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

    fn fresh_names_of_inode(&self, ino: u64) -> io::Result<Vec<Vec<u8>>> {
        ino::ino_paths(&self.new_fd, ino)
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
    ) -> io::Result<(HashSet<Vec<u8>>, HashSet<Vec<u8>>)> {
        let renames: RenameSeq = ops
            .iter()
            .filter(|o| o.kind == OpKind::Rename)
            .map(|o| (o.seq, o.path.clone(), o.aux.clone().unwrap_or_default()))
            .collect();

        let mut changed_inode: HashSet<Vec<u8>> = HashSet::new();
        let mut changed_positional: HashSet<Vec<u8>> = HashSet::new();
        let mut deleted: HashSet<Vec<u8>> = HashSet::new();
        let mut moves: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        for op in ops {
            let path = &op.path;
            match op.kind {
                OpKind::Create | OpKind::Modify => {
                    let f = final_name(&renames, path, op.seq);
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
        // inode's other names elsewhere in the tree.
        let mut expanded: HashSet<Vec<u8>> = HashSet::new();
        for p in &changed_inode {
            expanded.insert(p.clone());
            if let Some(m) = self.meta_new(p) {
                if m.typ == Typ::File && m.nlink > 1 {
                    for name in self.fresh_names_of_inode(m.ino)? {
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

        let changed: HashSet<Vec<u8>> =
            changed_inode.union(&changed_positional).cloned().collect();
        let mut final_deleted: HashSet<Vec<u8>> = HashSet::new();
        for p in &deleted {
            if self.in_old(p) && !self.in_new(p) && !changed.contains(p) {
                final_deleted.insert(p.clone());
            }
        }
        Ok((changed, final_deleted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ren(seq: u64, src: &str, dst: &str) -> (u64, Vec<u8>, Vec<u8>) {
        (seq, src.as_bytes().to_vec(), dst.as_bytes().to_vec())
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
