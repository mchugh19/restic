//go:build !linux

package main

import (
	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/fs"
)

// prepareBtrfsRun is a stub on non-Linux platforms: no btrfs
// provenance, no sparse plan.
func prepareBtrfsRun(_ *fs.LocalBtrfs, _ *data.Snapshot, _ []string, _ BackupOptions, _ func(string, ...interface{})) ([]data.BtrfsProvenance, *archiver.SparsePlan) {
	return nil, nil
}
