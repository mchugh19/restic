//go:build !linux

package fs

import "github.com/restic/restic/internal/errors"

// LocalBtrfs is only implemented on Linux; see fs_local_btrfs.go.
type LocalBtrfs struct {
	FS
}

// BtrfsSnapInfo mirrors the Linux definition.
type BtrfsSnapInfo struct {
	SubvolRoot     string
	SnapPath       string
	SourceUUID     string
	SnapParentUUID string
	SnapGeneration uint64
	SnapID         uint64
}

// NewLocalBtrfs always fails on non-Linux platforms.
func NewLocalBtrfs(_ []string, _ ErrorHandler, _ MessageHandler) (*LocalBtrfs, error) {
	return nil, errors.New("btrfs snapshots are only supported on Linux")
}

// Snapshots returns nil on non-Linux platforms.
func (fs *LocalBtrfs) Snapshots() []BtrfsSnapInfo { return nil }

// InfoFor returns false on non-Linux platforms.
func (fs *LocalBtrfs) InfoFor(string) (BtrfsSnapInfo, bool) { return BtrfsSnapInfo{}, false }

// DeleteSnapshots is a no-op on non-Linux platforms.
func (fs *LocalBtrfs) DeleteSnapshots() {}
