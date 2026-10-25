//go:build linux

// Package btrfs provides the kernel-interface pieces of the btrfs
// integration: a direct BTRFS_IOC_TREE_SEARCH_V2 walk over a
// snapshot's inode items, and BTRFS_IOC_INO_PATHS resolution.
//
// The semantics implemented here were settled empirically in the
// restic-btrfs research testbed (guest kernel 6.1, 2026-10-25):
//
//   - An inode counts as changed since generation G iff the transid
//     field inside its INODE_ITEM body (offset 8) is > G. The search
//     header's transid is the containing leaf block's last-COW stamp,
//     which the UAPI header documents as unrelated to any individual
//     item; it is only used for the conservative set below.
//   - The conservative set D2 collects every inode that has any item
//     in a leaf whose header stamp is > G. Changes that never touch
//     the inode item (xattrs, a directory's DIR_ITEM/DIR_INDEX
//     entries) still COW the containing leaf, so they land in D2.
//   - The search key is a single lexicographic (objectid, type,
//     offset) range; item types cannot be isolated via min/max_type
//     across objectids, so the walk spans all types and filters
//     client-side.
package btrfs

import (
	"encoding/binary"
	"fmt"
	"os"
	"unsafe"

	"golang.org/x/sys/unix"
)

const ioctlMagic = 0x94

// ioc computes a _IOWR ioctl number the way the kernel headers do. The
// size field must be the size of the buffer actually passed: the
// kernel distinguishes TREE_SEARCH from TREE_SEARCH_V2 (both nr 17)
// by it.
func ioc(nr, size uintptr) uintptr {
	return (3 << 30) | (size << 16) | (ioctlMagic << 8) | nr
}

const (
	iocTreeSearchV2 = 17
	iocInoPaths     = 35
)

const (
	itemTypeInodeItem = 1
)

// searchKey mirrors struct btrfs_ioctl_search_key (104 bytes).
type searchKey struct {
	TreeID     uint64
	MinObjID   uint64
	MaxObjID   uint64
	MinOffset  uint64
	MaxOffset  uint64
	MinTransID uint64
	MaxTransID uint64
	MinType    uint32
	MaxType    uint32
	NrItems    uint32
	Unused     uint32
	Unused1    [4]uint64
}

const searchKeySize = 104 // unsafe.Sizeof(searchKey{}); asserted in init

func init() {
	if unsafe.Sizeof(searchKey{}) != searchKeySize {
		panic("btrfs: searchKey layout drifted from the kernel UAPI")
	}
}

// InodeInfo is what the walk records per inode.
type InodeInfo struct {
	TransID    uint64 // transid field of the INODE_ITEM body
	Generation uint64 // generation field of the INODE_ITEM body
	Mode       uint32 // mode field of the INODE_ITEM body (offset 52)
	LeafStamp  uint64 // max header (leaf COW) transid over all items of this inode
}

// IsDir reports whether the inode is a directory.
func (i *InodeInfo) IsDir() bool { return i.Mode&0o170000 == 0o040000 }

// WalkInodeItems streams every item of the tree rooted at treeID,
// reachable through fd (any fd inside the filesystem; treeID selects
// the tree), and returns per-inode records for all inodes that have an
// INODE_ITEM. minTransID is passed to the kernel as a leaf-level
// prefilter; callers that need leaf stamps for the conservative set
// (ChangedSets) pass 0, because the filter would drop the very leaves
// whose stamps they need to observe for unchanged inodes.
func WalkInodeItems(fd uintptr, treeID uint64) (map[uint64]*InodeInfo, error) {
	out := make(map[uint64]*InodeInfo)
	seenInode := make(map[uint64]bool)
	const bufSize = 4 << 20
	const hdrSize = searchKeySize + 8 // key + buf_size
	var minObj, minOff uint64
	var minType uint32
	for {
		buf := make([]byte, hdrSize+bufSize)
		key := (*searchKey)(unsafe.Pointer(&buf[0]))
		key.TreeID = treeID
		key.MinObjID = minObj
		key.MaxObjID = ^uint64(0)
		key.MinOffset = minOff
		key.MaxOffset = ^uint64(0)
		key.MinTransID = 0
		key.MaxTransID = ^uint64(0)
		key.MinType = minType
		key.MaxType = 255
		key.NrItems = 1<<31 - 1
		binary.LittleEndian.PutUint64(buf[searchKeySize:], bufSize)

		_, _, errno := unix.Syscall(unix.SYS_IOCTL, fd,
			ioc(iocTreeSearchV2, uintptr(len(buf))),
			uintptr(unsafe.Pointer(&buf[0])))
		if errno != 0 {
			return nil, fmt.Errorf("btrfs tree search: %w", errno)
		}
		n := key.NrItems
		if n == 0 {
			break
		}
		off := hdrSize
		var lastObj, lastOff uint64
		var lastType uint32
		for i := uint32(0); i < n; i++ {
			hdrTrans := binary.LittleEndian.Uint64(buf[off:])
			oid := binary.LittleEndian.Uint64(buf[off+8:])
			ioff := binary.LittleEndian.Uint64(buf[off+16:])
			itype := binary.LittleEndian.Uint32(buf[off+24:])
			ilen := int(binary.LittleEndian.Uint32(buf[off+28:]))
			info := out[oid]
			if info == nil {
				info = &InodeInfo{}
				out[oid] = info
			}
			if hdrTrans > info.LeafStamp {
				info.LeafStamp = hdrTrans
			}
			if itype == itemTypeInodeItem && ilen >= 56 {
				body := buf[off+32:]
				info.Generation = binary.LittleEndian.Uint64(body)
				info.TransID = binary.LittleEndian.Uint64(body[8:])
				info.Mode = binary.LittleEndian.Uint32(body[52:])
				seenInode[oid] = true
			}
			lastObj, lastType, lastOff = oid, itype, ioff
			off += 32 + ilen
		}
		// Advance the lexicographic key past the last item returned.
		minObj, minType, minOff = lastObj, lastType, lastOff+1
		if minOff == 0 {
			minType++
			if minType > 255 {
				minObj, minType = minObj+1, 0
			}
		}
	}
	// Only inodes that actually have an INODE_ITEM are meaningful to
	// callers; items of other types can reference objectids that are
	// not inodes (e.g. extent items keyed by bytenr).
	for oid := range out {
		if !seenInode[oid] {
			delete(out, oid)
		}
	}
	return out, nil
}

// ChangedSets returns D1 (INODE_ITEM transid > gPrev) and D2 (any item
// of the inode in a leaf stamped > gPrev) for the snapshot tree
// reachable through fd. D2 is always a superset of D1.
func ChangedSets(fd uintptr, treeID, gPrev uint64) (d1, d2 map[uint64]bool, err error) {
	items, err := WalkInodeItems(fd, treeID)
	if err != nil {
		return nil, nil, err
	}
	d1 = make(map[uint64]bool)
	d2 = make(map[uint64]bool)
	for oid, info := range items {
		if info.TransID > gPrev {
			d1[oid] = true
			d2[oid] = true
		} else if info.LeafStamp > gPrev {
			d2[oid] = true
		}
	}
	return d1, d2, nil
}

// InoPaths resolves an inode number to its paths relative to the root
// of the subvolume that fd belongs to, via BTRFS_IOC_INO_PATHS. It
// needs CAP_DAC_READ_SEARCH (root in practice). The kernel clamps the
// result buffer to 4 KiB and reports the shortfall instead of
// truncating; any shortfall is an error here, because a partial path
// list must never be mistaken for the full one.
func InoPaths(fd uintptr, ino uint64) ([]string, error) {
	// struct btrfs_ioctl_ino_path_args (56 bytes): inum, size,
	// reserved[4], fspath pointer at offset 48.
	args := make([]byte, 56)
	binary.LittleEndian.PutUint64(args[0:], ino)
	const contSize = 4096
	cont := make([]byte, contSize)
	binary.LittleEndian.PutUint64(args[8:], contSize)
	binary.LittleEndian.PutUint64(args[48:], uint64(uintptr(unsafe.Pointer(&cont[0]))))

	_, _, errno := unix.Syscall(unix.SYS_IOCTL, fd,
		ioc(iocInoPaths, uintptr(len(args))),
		uintptr(unsafe.Pointer(&args[0])))
	if errno != 0 {
		return nil, fmt.Errorf("btrfs ino_paths(%d): %w", ino, errno)
	}
	// struct btrfs_data_container: bytes_left, bytes_missing, elem_cnt,
	// elem_missed (u32 each), then val[] of u64 offsets to C strings,
	// each offset relative to the container start.
	missing := binary.LittleEndian.Uint32(cont[4:])
	if missing != 0 {
		return nil, fmt.Errorf("btrfs ino_paths(%d): result needs %d more bytes (kernel 4 KiB clamp)", ino, missing)
	}
	cnt := int(binary.LittleEndian.Uint32(cont[8:]))
	paths := make([]string, 0, cnt)
	for i := 0; i < cnt; i++ {
		// Each val is an offset relative to the start of the val
		// array (container + 16), verified against the kernel's
		// end-packed string layout in the testbed.
		rel := 16 + int(binary.LittleEndian.Uint64(cont[16+i*8:]))
		if rel < 16 || rel >= contSize {
			return nil, fmt.Errorf("btrfs ino_paths(%d): bogus path offset %d", ino, rel)
		}
		end := rel
		for end < contSize && cont[end] != 0 {
			end++
		}
		paths = append(paths, string(cont[rel:end]))
	}
	return paths, nil
}

// OpenRoot opens a directory (typically a snapshot root) for use with
// the queries above.
func OpenRoot(path string) (*os.File, error) {
	return os.Open(path)
}
