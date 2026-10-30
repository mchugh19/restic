//! BTRFS_IOC_TREE_SEARCH_V2: one bulk pass over a snapshot's
//! metadata tree, building the map the atime filter needs:
//! inode number -> mtime of that inode's INODE_ITEM.
//!
//! This replaces the per-candidate retained-tree lstat in the
//! filter's proof (see resolve.rs and TREESEARCH_DESIGN.md).
//! An INODE_ITEM's mtime field is the same fact an lstat of
//! that inode returns; reading the items in one sequential
//! B-tree walk costs one bulk transfer per few thousand items
//! instead of one cold pathname lookup per candidate.
//!
//! The walk mirrors internal/btrfs in this repository, whose
//! implementation the Project 2 gates validated, and the raw
//! ioctl idiom of ino.rs in this crate:
//!
//! - Request number 17 under magic 0x94; the kernel tells
//!   TREE_SEARCH from TREE_SEARCH_V2 by the size field of the
//!   request, which must be the size of the whole argument
//!   buffer actually passed (search key + buf_size + items).
//! - The search key is a single lexicographic (objectid, type,
//!   offset) range, so min/max types cannot isolate INODE_ITEMs
//!   across objectids: the walk spans every item type and
//!   filters client-side.
//! - Decisions use fields inside the INODE_ITEM body, never
//!   the per-item search header (whose transid is the leaf
//!   block's last-COW stamp). INODE_ITEM body: generation @0,
//!   transid @8, size @16, nbytes @24, block_group @32, nlink
//!   @40, uid @44, gid @48, mode @52, rdev @56, flags @64,
//!   sequence @72, reserved @80..112, atime @112, ctime @124,
//!   mtime @136 (i64 sec, u32 nsec @144), otime @148; the body
//!   is 160 bytes, and a shorter item is not decoded.
//!
//! Needs CAP_SYS_ADMIN (root in practice), like the rest of
//! the producer. Any failure is an Err to the caller, which
//! treats it as "no map" and falls back to the per-path stat
//! proof; this module never panics on kernel-supplied data.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn ioctl(fd: i32, request: u64, arg: *mut c_void) -> i32;
}

const BTRFS_IOCTL_MAGIC: u64 = 0x94;
const IOC_TREE_SEARCH_NR: u64 = 17;
const ITEM_TYPE_INODE_ITEM: u32 = 1;

const KEY_LEN: usize = 104;
const HDR_LEN: usize = KEY_LEN + 8; // search key + u64 buf_size
const BUF_SIZE: usize = 4 * 1024 * 1024;
const ITEM_HDR_LEN: usize = 32;
const INODE_ITEM_LEN: usize = 160;
const MTIME_SEC_OFF: usize = 136;
const MTIME_NSEC_OFF: usize = 144;

// Search-key field offsets within the 104-byte key.
const K_TREE_ID: usize = 0;
const K_MIN_OBJECTID: usize = 8;
const K_MAX_OBJECTID: usize = 16;
const K_MIN_OFFSET: usize = 24;
const K_MAX_OFFSET: usize = 32;
const K_MIN_TRANSID: usize = 40;
const K_MAX_TRANSID: usize = 48;
const K_MIN_TYPE: usize = 56;
const K_MAX_TYPE: usize = 60;
const K_NR_ITEMS: usize = 64;

fn ioc_tree_search_v2(total: usize) -> u64 {
    // _IOWR(magic, nr, args): direction 3 (read|write); the
    // size field is the whole argument buffer, which is how
    // the kernel selects the V2 form.
    (3u64 << 30) | ((total as u64) << 16) | (BTRFS_IOCTL_MAGIC << 8)
        | IOC_TREE_SEARCH_NR
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// The mtime carried by an INODE_ITEM body, as (sec, nsec).
/// None when the body is too short to hold the field.
fn inode_item_mtime(body: &[u8]) -> Option<(i64, i64)> {
    if body.len() < INODE_ITEM_LEN {
        return None;
    }
    let sec = i64::from_le_bytes(
        body[MTIME_SEC_OFF..MTIME_SEC_OFF + 8].try_into().ok()?,
    );
    let nsec = u32::from_le_bytes(
        body[MTIME_NSEC_OFF..MTIME_NSEC_OFF + 4].try_into().ok()?,
    );
    Some((sec, nsec as i64))
}

/// Scan one filled result buffer: insert an entry for every
/// INODE_ITEM it holds and return the (objectid, type, offset)
/// of the last item, whatever its type, so the caller can
/// advance the search key past it. `nr_items` is the count the
/// kernel wrote back into the search key. A buffer whose items
/// run past its end is an error: the caller treats it as "no
/// map" rather than trusting a half-parsed walk.
fn scan_buffer(
    items: &[u8],
    nr_items: u32,
    map: &mut HashMap<u64, (i64, i64)>,
) -> io::Result<Option<(u64, u32, u64)>> {
    let mut pos = 0usize;
    let mut last = None;
    for _ in 0..nr_items {
        if items.len() - pos < ITEM_HDR_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree search: item header overruns the result buffer",
            ));
        }
        let objectid = get_u64(items, pos + 8);
        let offset = get_u64(items, pos + 16);
        let itype = get_u32(items, pos + 24);
        let ilen = get_u32(items, pos + 28) as usize;
        if items.len() - pos - ITEM_HDR_LEN < ilen {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree search: item body overruns the result buffer",
            ));
        }
        if itype == ITEM_TYPE_INODE_ITEM {
            if let Some(mt) =
                inode_item_mtime(&items[pos + ITEM_HDR_LEN..pos + ITEM_HDR_LEN + ilen])
            {
                map.insert(objectid, mt);
            }
        }
        last = Some((objectid, itype, offset));
        pos += ITEM_HDR_LEN + ilen;
    }
    Ok(last)
}

/// Every inode's mtime in the tree of the subvolume `root` is
/// an open handle on (tree_id 0: the fd's own tree). One pass,
/// bulk buffers; the result is keyed purely by inode number.
pub fn inode_mtimes(root: &File) -> io::Result<HashMap<u64, (i64, i64)>> {
    let total = HDR_LEN + BUF_SIZE;
    let mut map = HashMap::new();
    let mut buf = vec![0u8; total];
    let (mut min_objectid, mut min_type, mut min_offset) = (0u64, 0u32, 0u64);
    loop {
        for b in buf.iter_mut() {
            *b = 0;
        }
        put_u64(&mut buf, K_TREE_ID, 0);
        put_u64(&mut buf, K_MIN_OBJECTID, min_objectid);
        put_u64(&mut buf, K_MAX_OBJECTID, u64::MAX);
        put_u64(&mut buf, K_MIN_OFFSET, min_offset);
        put_u64(&mut buf, K_MAX_OFFSET, u64::MAX);
        put_u64(&mut buf, K_MIN_TRANSID, 0);
        put_u64(&mut buf, K_MAX_TRANSID, u64::MAX);
        put_u32(&mut buf, K_MIN_TYPE, min_type);
        put_u32(&mut buf, K_MAX_TYPE, 255);
        put_u32(&mut buf, K_NR_ITEMS, (1 << 31) - 1);
        put_u64(&mut buf, KEY_LEN, BUF_SIZE as u64);
        let rc = unsafe {
            ioctl(
                root.as_raw_fd(),
                ioc_tree_search_v2(total),
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let nr_items = get_u32(&buf, K_NR_ITEMS);
        if nr_items == 0 {
            break;
        }
        let last = scan_buffer(&buf[HDR_LEN..], nr_items, &mut map)?;
        let Some((oid, itype, ioff)) = last else {
            break;
        };
        // Advance the lexicographic key past the last returned
        // item, as internal/btrfs does. A key that fails to
        // advance would loop forever on a misbehaving kernel;
        // refuse instead (the caller falls back to stats).
        let prev = (min_objectid, min_type, min_offset);
        min_objectid = oid;
        min_type = itype;
        min_offset = ioff.wrapping_add(1);
        if min_offset == 0 {
            min_type = min_type.wrapping_add(1);
            if min_type == 0 {
                min_objectid = min_objectid.wrapping_add(1);
            }
        }
        if (min_objectid, min_type, min_offset) == prev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree search: search key failed to advance",
            ));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(oid: u64, itype: u32, offset: u64, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u64.to_le_bytes()); // header transid
        v.extend_from_slice(&oid.to_le_bytes());
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&itype.to_le_bytes());
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn inode_body(mtime_sec: i64, mtime_nsec: u32) -> Vec<u8> {
        let mut body = vec![0u8; INODE_ITEM_LEN];
        body[MTIME_SEC_OFF..MTIME_SEC_OFF + 8]
            .copy_from_slice(&mtime_sec.to_le_bytes());
        body[MTIME_NSEC_OFF..MTIME_NSEC_OFF + 4]
            .copy_from_slice(&mtime_nsec.to_le_bytes());
        body
    }

    #[test]
    fn extracts_mtime_from_inode_items_only() {
        let mut items = Vec::new();
        items.extend(item(257, 1, 0, &inode_body(1_700_000_001, 111)));
        items.extend(item(258, 25, 4096, &[7u8; 16])); // extent item
        items.extend(item(259, 1, 0, &inode_body(-5, 999_999_999)));
        let mut map = HashMap::new();
        let last = scan_buffer(&items, 3, &mut map).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&257), Some(&(1_700_000_001, 111)));
        assert_eq!(map.get(&259), Some(&(-5, 999_999_999)));
        assert_eq!(last, Some((259, 1, 0)));
    }

    #[test]
    fn short_inode_item_is_skipped_not_decoded() {
        let mut items = Vec::new();
        items.extend(item(300, 1, 0, &[0u8; 56])); // too short
        let mut map = HashMap::new();
        scan_buffer(&items, 1, &mut map).unwrap();
        assert!(map.is_empty(), "short item must not enter the map");
    }

    #[test]
    fn overrunning_buffer_is_an_error() {
        let mut items = Vec::new();
        items.extend(item(257, 1, 0, &inode_body(1, 2)));
        // Claim a second item that is not there.
        assert!(scan_buffer(&items, 2, &mut HashMap::new()).is_err());
    }
}
