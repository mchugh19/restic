//go:build linux

package fs

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"

	"github.com/restic/restic/internal/errors"
	"golang.org/x/sys/unix"
)

const btrfsSuperMagic = 0x9123683e

// BtrfsSnapInfo describes one restic-managed read-only snapshot of a
// source subvolume.
type BtrfsSnapInfo struct {
	SubvolRoot     string // original path of the source subvolume root
	SnapPath       string // where the read-only snapshot lives
	SourceUUID     string // UUID of the source subvolume
	SnapParentUUID string // parent_uuid of the snapshot (== SourceUUID)
	SnapGeneration uint64 // generation of the snapshot subvolume
	SnapID         uint64 // subvolume ID of the snapshot (its btrfs tree ID)
}

// LocalBtrfs is a wrapper around the local file system which reads
// from read-only btrfs snapshots in a transparent way, mirroring
// LocalVss: the archiver records the user's original paths while all
// filesystem access is redirected into the snapshots. Snapshot
// lifecycle is owned by the caller (the backup command), which must
// call DeleteSnapshots when the run ends.
//
// Snapshot creation shells out to btrfs-progs: lifecycle operations
// run once per target per backup, and the prototype values correctness
// over the dependency. The sparse-scan tree search is in-process (see
// internal/btrfs).
type LocalBtrfs struct {
	FS
	snaps      []BtrfsSnapInfo
	byRoot     map[string]string // source subvolume root -> snapshot path
	msgError   ErrorHandler
	msgMessage MessageHandler
	mutex      sync.Mutex
}

// statically ensure that LocalBtrfs implements FS.
var _ FS = &LocalBtrfs{}

// subvolumeRoot resolves path to the root of its containing btrfs
// subvolume: the path itself must be on btrfs, and the subvolume root
// is the nearest ancestor (or the path itself) whose inode is 256.
func subvolumeRoot(path string) (string, error) {
	var stfs unix.Statfs_t
	if err := unix.Statfs(path, &stfs); err != nil {
		return "", err
	}
	if stfs.Type != btrfsSuperMagic {
		return "", fmt.Errorf("%v is not on a btrfs filesystem", path)
	}
	cur := path
	for {
		fi, err := os.Lstat(cur)
		if err != nil {
			return "", err
		}
		if st, ok := fi.Sys().(*syscall.Stat_t); ok && st.Ino == 256 {
			return cur, nil
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			return "", fmt.Errorf("no subvolume root found above %v", path)
		}
		cur = parent
	}
}

// subvolumeShow runs `btrfs subvolume show` and returns its key/value
// pairs.
func subvolumeShow(path string) (map[string]string, error) {
	out, err := exec.Command("btrfs", "subvolume", "show", path).Output()
	if err != nil {
		return nil, fmt.Errorf("btrfs subvolume show %v: %w", path, err)
	}
	kv := make(map[string]string)
	for _, line := range strings.Split(string(out), "\n") {
		k, v, ok := strings.Cut(line, ":")
		if ok {
			kv[strings.TrimSpace(k)] = strings.TrimSpace(v)
		}
	}
	return kv, nil
}

func showUint(kv map[string]string, key string) uint64 {
	n, _ := strconv.ParseUint(kv[key], 10, 64)
	return n
}

// sweepStale removes restic-managed snapshots left next to root by
// runs that were killed before their deferred cleanup could run. The
// caller (a fresh run that is about to create its own snapshot) is the
// only sweeper, so a live run's snapshots are never swept from under
// it as long as runs do not overlap on the same subvolume.
// sweepStale deletes leftover .restic-snap-* subvolumes next to root.
// keep holds snapshot paths already created by this run (for other
// subvolume roots sharing the same parent directory); those are not
// stale and must never be swept. Getting this wrong silently drops a
// sibling target's snapshot mid-run, so the keep set is load-bearing,
// not defensive.
func sweepStale(root string, msgError ErrorHandler, keep map[string]bool) {
	parent := filepath.Dir(root)
	entries, err := os.ReadDir(parent)
	if err != nil {
		return
	}
	for _, e := range entries {
		if !strings.HasPrefix(e.Name(), ".restic-snap-") {
			continue
		}
		p := filepath.Join(parent, e.Name())
		if keep[p] {
			continue
		}
		if err := exec.Command("btrfs", "subvolume", "delete", p).Run(); err != nil {
			msgError(p, errors.Errorf("failed to delete stale btrfs snapshot: %v", err))
		}
	}
}

// NewLocalBtrfs creates a read-only snapshot of the containing
// subvolume of every target (one snapshot per subvolume, shared by
// targets inside it) and returns the translating wrapper. It fails
// closed: any target that cannot be snapshotted is an error, never a
// silent fallback to reading the live tree.
func NewLocalBtrfs(targets []string, msgError ErrorHandler, msgMessage MessageHandler) (*LocalBtrfs, error) {
	lb := &LocalBtrfs{
		FS:         NewLocal(),
		byRoot:     make(map[string]string),
		msgError:   msgError,
		msgMessage: msgMessage,
	}
	roots := make(map[string]bool)
	for _, target := range targets {
		abs, err := filepath.Abs(target)
		if err != nil {
			return nil, err
		}
		// A target that is not on btrfs cannot be snapshotted; it is
		// backed up live (the wrapper passes its paths through
		// unchanged) and gets no provenance record. Mixed target lists
		// are the norm on real systems, so this must not fail the run.
		// A target that IS on btrfs but whose snapshot fails still
		// fails the run below: snapshotting was possible there and
		// silently reading a changing tree would be worse than failing.
		var stfs unix.Statfs_t
		if err := unix.Statfs(abs, &stfs); err != nil {
			return nil, err
		}
		if stfs.Type != btrfsSuperMagic {
			msgMessage("btrfs: %v is not on a btrfs filesystem, backing it up without a snapshot", abs)
			continue
		}
		root, err := subvolumeRoot(abs)
		if err != nil {
			return nil, err
		}
		if roots[root] {
			continue
		}
		roots[root] = true

		srcShow, err := subvolumeShow(root)
		if err != nil {
			return nil, err
		}
		keep := make(map[string]bool, len(lb.byRoot))
		for _, p := range lb.byRoot {
			keep[p] = true
		}
		sweepStale(root, msgError, keep)

		// The snapshot name carries the run's pid plus a per-root
		// sequence number: sibling subvolume roots share a parent
		// directory, and a pid-only name collides between them (the
		// second create then fails, or worse, the name is reused).
		snapPath := filepath.Join(filepath.Dir(root),
			fmt.Sprintf(".restic-snap-%d-%d", os.Getpid(), len(lb.snaps)))
		if err := exec.Command("btrfs", "subvolume", "snapshot", "-r", root, snapPath).Run(); err != nil {
			return nil, fmt.Errorf("btrfs subvolume snapshot of %v: %w", root, err)
		}
		// Commit the transaction so the snapshot's generation is final.
		_ = exec.Command("btrfs", "filesystem", "sync", root).Run()

		snapShow, err := subvolumeShow(snapPath)
		if err != nil {
			_ = exec.Command("btrfs", "subvolume", "delete", snapPath).Run()
			return nil, err
		}
		info := BtrfsSnapInfo{
			SubvolRoot:     root,
			SnapPath:       snapPath,
			SourceUUID:     srcShow["UUID"],
			SnapParentUUID: snapShow["Parent UUID"],
			SnapGeneration: showUint(snapShow, "Generation"),
			SnapID:         showUint(snapShow, "Subvolume ID"),
		}
		lb.snaps = append(lb.snaps, info)
		lb.byRoot[root] = snapPath
		msgMessage("created btrfs snapshot of %v at %v (generation %v)", root, snapPath, info.SnapGeneration)
	}
	return lb, nil
}

// Snapshots returns the snapshots created for this run.
func (fs *LocalBtrfs) Snapshots() []BtrfsSnapInfo { return fs.snaps }

// InfoFor returns the snapshot serving target (the target may be the
// subvolume root itself or anything beneath it).
func (fs *LocalBtrfs) InfoFor(target string) (BtrfsSnapInfo, bool) {
	abs, err := filepath.Abs(target)
	if err != nil {
		return BtrfsSnapInfo{}, false
	}
	best := -1
	var info BtrfsSnapInfo
	for _, s := range fs.snaps {
		if abs == s.SubvolRoot || strings.HasPrefix(abs, s.SubvolRoot+string(os.PathSeparator)) {
			if len(s.SubvolRoot) > best {
				best = len(s.SubvolRoot)
				info = s
			}
		}
	}
	return info, best >= 0
}

// DeleteSnapshots deletes all snapshots created for this run.
func (fs *LocalBtrfs) DeleteSnapshots() {
	fs.mutex.Lock()
	defer fs.mutex.Unlock()
	kept := fs.snaps[:0]
	for _, s := range fs.snaps {
		if err := exec.Command("btrfs", "subvolume", "delete", s.SnapPath).Run(); err != nil {
			fs.msgError(s.SnapPath, errors.Errorf("failed to delete btrfs snapshot: %v", err))
			kept = append(kept, s)
		}
	}
	fs.snaps = kept
}

// snapshotPath translates an original path into the snapshot serving
// its subvolume. Paths outside every snapshotted subvolume pass
// through unchanged.
func (fs *LocalBtrfs) snapshotPath(name string) string {
	clean := filepath.Clean(name)
	best := ""
	for root := range fs.byRoot {
		if clean == root || strings.HasPrefix(clean, root+string(os.PathSeparator)) {
			if len(root) > len(best) {
				best = root
			}
		}
	}
	if best == "" {
		return name
	}
	rel, err := filepath.Rel(best, clean)
	if err != nil {
		return name
	}
	return filepath.Join(fs.byRoot[best], rel)
}

// OpenFile wraps the OpenFile method of the underlying file system.
func (fs *LocalBtrfs) OpenFile(name string, flag int, metadataOnly bool) (File, error) {
	return fs.FS.OpenFile(fs.snapshotPath(name), flag, metadataOnly)
}

// Lstat wraps the Lstat method of the underlying file system.
func (fs *LocalBtrfs) Lstat(name string) (*ExtendedFileInfo, error) {
	return fs.FS.Lstat(fs.snapshotPath(name))
}
