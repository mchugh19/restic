//! BTRFS_IOC_INO_PATHS: resolve an inode number to all of its
//! paths within one subvolume. This is the one piece of filesystem
//! state the send stream cannot supply on its own: when one name of
//! a multiply-linked file changes, the stream names a single
//! representative path, but the change applies to the inode and so
//! to every name. A targeted ioctl per changed inode replaces the
//! prototype parser's full metadata walks of both snapshots.
//!
//! The layout mirrors internal/btrfs in this repository, whose
//! implementation the project 2 gates validated: request number 35
//! under magic 0x94, a 56-byte argument struct (inum, size,
//! reserved[4], fspath pointer at offset 48), and a result
//! container the kernel fills with as many paths as it can.
//!
//! The container has a hard kernel-side limit: the kernel fills at
//! most one page of it, however large a buffer the caller supplies
//! (verified empirically on kernel 6.17: elem_cnt, elem_missed and
//! bytes_missing are identical for buffers from 4 KiB to 1 MiB).
//! An inode whose paths do not fit in that page is reported as
//! `InoResolution::Clamped`, never as a partial list: a truncated
//! sibling set would silently drop a name from the changed list.
//! The caller resolves clamped inodes another way (a metadata walk
//! of the snapshot; see resolve.rs).
//!
//! Needs CAP_DAC_READ_SEARCH (root in practice), like the rest of
//! the producer.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> i32;
}

const BTRFS_IOCTL_MAGIC: u64 = 0x94;
const IOC_INO_PATHS_NR: u64 = 35;
const ARGS_LEN: u64 = 56;
const CONTAINER_LEN: usize = 4096;

fn ioc_ino_paths() -> u64 {
    // _IOWR(magic, nr, args): direction 3 (read|write).
    (3u64 << 30) | (ARGS_LEN << 16) | (BTRFS_IOCTL_MAGIC << 8) | IOC_INO_PATHS_NR
}

/// The outcome of an inode-to-paths lookup.
pub enum InoResolution {
    /// Every path of the inode, relative to the root of the
    /// subvolume that `root` belongs to. An inode with no paths in
    /// this subvolume (fully deleted between the snapshots, or a
    /// lookup the kernel rejects with ENOENT / EINVAL / ENODATA)
    /// yields an empty list; that case is a normal outcome of the
    /// delete-side sibling rule, not a failure.
    Complete(Vec<Vec<u8>>),
    /// The inode has more paths than fit in the kernel's one-page
    /// container; the paths that did fit are discarded. The caller
    /// must resolve this inode another way.
    Clamped,
}

/// All paths of `ino` within the subvolume that `root` (an open
/// directory handle on it) belongs to, or `InoResolution::Clamped`
/// when the kernel could not fit them all. Any other error is
/// fatal to the caller.
pub fn ino_paths(root: &File, ino: u64) -> io::Result<InoResolution> {
    // struct btrfs_ioctl_ino_path_args (56 bytes): inum u64, size
    // u32 (+4 padding), reserved u64[4], fspath pointer at 48.
    let mut container = vec![0u8; CONTAINER_LEN];
    let mut args = [0u8; 56];
    args[0..8].copy_from_slice(&ino.to_le_bytes());
    args[8..12].copy_from_slice(&(CONTAINER_LEN as u32).to_le_bytes());
    args[48..56].copy_from_slice(&(container.as_mut_ptr() as u64).to_le_bytes());
    let rc = unsafe {
        ioctl(
            root.as_raw_fd(),
            ioc_ino_paths(),
            args.as_mut_ptr() as *mut c_void,
        )
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // ENOENT, EINVAL, ENODATA: no such inode here.
            Some(2) | Some(22) | Some(61) => return Ok(InoResolution::Complete(Vec::new())),
            _ => return Err(e),
        }
    }
    // struct btrfs_data_container: bytes_left u32, bytes_missing
    // u32, elem_cnt u32, elem_missed u32, then val[] of u64 offsets
    // to NUL-terminated strings, each offset relative to the start
    // of the val array (container + 16).
    let missing = u32::from_le_bytes(container[4..8].try_into().unwrap());
    let missed = u32::from_le_bytes(container[12..16].try_into().unwrap());
    if missing != 0 || missed != 0 {
        return Ok(InoResolution::Clamped);
    }
    let cnt = u32::from_le_bytes(container[8..12].try_into().unwrap()) as usize;
    let mut paths = Vec::with_capacity(cnt);
    for i in 0..cnt {
        let off = u64::from_le_bytes(
            container[16 + i * 8..16 + i * 8 + 8].try_into().unwrap(),
        ) as usize;
        let start = 16 + off;
        if start < 16 || start >= CONTAINER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ino_paths({ino}): bogus path offset {off}"),
            ));
        }
        let mut end = start;
        while end < CONTAINER_LEN && container[end] != 0 {
            end += 1;
        }
        paths.push(container[start..end].to_vec());
    }
    Ok(InoResolution::Complete(paths))
}
