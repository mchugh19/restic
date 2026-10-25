//go:build linux

package fs

import (
	"fmt"
	"io"
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
	links      map[string]string // absolute target as spelled -> resolved target
	msgError   ErrorHandler
	msgMessage MessageHandler
	mutex      sync.Mutex
}

// resolveLinks rewrites the longest symlinked-target prefix of name
// to its resolved form. Paths that do not pass through a symlinked
// target are returned unchanged.
func (fs *LocalBtrfs) resolveLinks(name string) string {
	best := ""
	for spelled := range fs.links {
		if name == spelled || strings.HasPrefix(name, spelled+string(os.PathSeparator)) {
			if len(spelled) > len(best) {
				best = spelled
			}
		}
	}
	if best == "" {
		return name
	}
	rel, err := filepath.Rel(best, name)
	if err != nil {
		return name
	}
	return filepath.Join(fs.links[best], rel)
}

// ResolvedTarget returns target in absolute form with a symlinked
// target prefix resolved. The backup command uses it to address a
// target inside its subvolume; records and plans stay keyed by the
// spelled form (see ResolvedTarget's callers).
func (fs *LocalBtrfs) ResolvedTarget(target string) string {
	abs, err := filepath.Abs(target)
	if err != nil {
		return target
	}
	return fs.resolveLinks(abs)
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

// sweepStale removes restic-managed snapshots left inside root by
// runs that were killed before their deferred cleanup could run. The
// caller (a fresh run that is about to create its own snapshot) is the
// only sweeper, so a live run's snapshots are never swept from under
// it as long as runs do not overlap on the same subvolume.
// keep holds snapshot paths already created by this run; those are
// not stale and must never be swept.
func sweepStale(root string, msgError ErrorHandler, keep map[string]bool) {
	entries, err := os.ReadDir(root)
	if err != nil {
		return
	}
	for _, e := range entries {
		if !strings.HasPrefix(e.Name(), ".restic-snap-") {
			continue
		}
		p := filepath.Join(root, e.Name())
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
		links:      make(map[string]string),
		msgError:   msgError,
		msgMessage: msgMessage,
	}
	roots := make(map[string]bool)
	for _, target := range targets {
		abs, err := filepath.Abs(target)
		if err != nil {
			return nil, err
		}
		// A target spelled through a symlink is resolved before
		// anything looks at the filesystem: Statfs would follow the
		// link, but the subvolume climb and the path translation work
		// on literal prefixes, so without resolution the target's
		// subvolume is never found. Recorded paths are unaffected;
		// they always use the spelling the user gave.
		resolved := abs
		if r, err := filepath.EvalSymlinks(abs); err == nil {
			resolved = r
		}
		if resolved != abs {
			lb.links[abs] = resolved
		}
		// A target that is not on btrfs cannot be snapshotted; it is
		// backed up live (the wrapper passes its paths through
		// unchanged) and gets no provenance record. Mixed target lists
		// are the norm on real systems, so this must not fail the run.
		// A target that IS on btrfs but whose snapshot fails still
		// fails the run below: snapshotting was possible there and
		// silently reading a changing tree would be worse than failing.
		var stfs unix.Statfs_t
		if err := unix.Statfs(resolved, &stfs); err != nil {
			return nil, err
		}
		if stfs.Type != btrfsSuperMagic {
			msgMessage("btrfs: %v is not on a btrfs filesystem, backing it up without a snapshot", abs)
			continue
		}
		root, err := subvolumeRoot(resolved)
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

		// The snapshot lives INSIDE the subvolume root, not next to
		// it: the root may be a mountpoint whose parent directory is
		// on a different filesystem (a top-level subvolume mounted
		// at /srv/mirror, with /srv on ext4, is the common case),
		// and btrfs snapshots must be created on the filesystem they
		// snapshot. The name carries the run's pid plus a per-root
		// sequence number so concurrent or sibling roots can never
		// collide. The snapshot's placeholder entry inside its own
		// tree is hidden by OpenFile when the root itself is listed.
		snapPath := filepath.Join(root,
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
	abs = fs.resolveLinks(abs)
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
	clean := fs.resolveLinks(filepath.Clean(name))
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

// snapRootFile hides a snapshot's own placeholder entry from listings
// of the snapshot root. The snapshot lives inside the source subvolume,
// so the snapshot's tree contains its own name as an empty directory
// (the usual nested-subvolume placeholder); it is not part of the
// user's tree and must not be recorded when a target is the subvolume
// root itself.
type snapRootFile struct {
	File
	drop string
}

func (f *snapRootFile) Readdirnames(n int) ([]string, error) {
	kept := func(names []string) []string {
		out := names[:0]
		for _, name := range names {
			if name != f.drop {
				out = append(out, name)
			}
		}
		return out
	}
	if n <= 0 {
		names, err := f.File.Readdirnames(n)
		return kept(names), err
	}
	var out []string
	for {
		names, err := f.File.Readdirnames(n)
		out = append(out, kept(names)...)
		if err != nil || len(out) > 0 {
			return out, err
		}
		if len(names) == 0 {
			return out, io.EOF
		}
	}
}

// OpenFile wraps the OpenFile method of the underlying file system.
func (fs *LocalBtrfs) OpenFile(name string, flag int, metadataOnly bool) (File, error) {
	p := fs.snapshotPath(name)
	file, err := fs.FS.OpenFile(p, flag, metadataOnly)
	if err != nil {
		return nil, err
	}
	for _, snap := range fs.byRoot {
		if p == snap {
			return &snapRootFile{File: file, drop: filepath.Base(snap)}, nil
		}
	}
	return file, nil
}

// Lstat wraps the Lstat method of the underlying file system.
func (fs *LocalBtrfs) Lstat(name string) (*ExtendedFileInfo, error) {
	return fs.FS.Lstat(fs.snapshotPath(name))
}
