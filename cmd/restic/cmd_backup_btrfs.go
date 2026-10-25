//go:build linux

package main

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/btrfs"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/feature"
	"github.com/restic/restic/internal/fs"
)

// btrfsConfigHash fingerprints the inputs that decide whether a sparse
// walk of target produces the same tree as a full walk: the target
// itself, its exclude patterns and the change-detection flags.
func btrfsConfigHash(target string, opts BackupOptions) string {
	h := sha256.New()
	fmt.Fprintf(h, "target=%s\n", target)
	ex := append([]string{}, opts.Excludes...)
	sort.Strings(ex)
	for _, e := range ex {
		fmt.Fprintf(h, "exclude=%s\n", e)
	}
	fmt.Fprintf(h, "ignore-ctime=%v ignore-inode=%v with-atime=%v\n",
		opts.IgnoreCtime, opts.IgnoreInode, opts.WithAtime)
	return hex.EncodeToString(h.Sum(nil))
}

// prepareBtrfsRun computes the provenance records this run will store
// and, when every guard passes, the sparse plan for the archiver. Any
// guard failure leaves the plan nil, which the archiver treats as a
// full walk; the reason is reported through logf. Falling back is
// always safe, so this function never fails the backup.
func prepareBtrfsRun(bfs *fs.LocalBtrfs, parent *data.Snapshot, targets []string, opts BackupOptions, logf func(string, ...interface{})) ([]data.BtrfsProvenance, *archiver.SparsePlan) {
	records := make([]data.BtrfsProvenance, 0, len(targets))
	sparseOK := feature.Flag.Enabled(feature.BtrfsSparseScan) && parent != nil
	if opts.WithAtime {
		// atime-only changes are not reliably stamped in inode items.
		sparseOK = false
	}
	listDirs := make(map[string]bool)
	var analysedRoots []string

	for _, target := range targets {
		info, ok := bfs.InfoFor(target)
		if !ok {
			// Not on btrfs (read live): no record, no plan entries,
			// and crucially no veto over the other targets. The
			// archiver only splices below analysed roots, so this
			// target's directories are always listed.
			continue
		}
		// Records and the configuration hash are keyed by the
		// target's absolute form. Keying by the string as typed
		// would let a spelling variant (a trailing slash, a "."
		// component) miss the parent's record and cost a full
		// walk; the absolute form is the identity the archiver
		// and the recorded snapshot paths already use.
		key, err := filepath.Abs(target)
		if err != nil {
			key = filepath.Clean(target)
		}
		analysedRoots = append(analysedRoots, key)
		hash := btrfsConfigHash(key, opts)
		records = append(records, data.BtrfsProvenance{
			Path:       key,
			SubvolUUID: info.SourceUUID,
			Generation: info.SnapGeneration,
			Complete:   true,
			ConfigHash: hash,
		})
		if !sparseOK {
			continue
		}
		var prec *data.BtrfsProvenance
		for i := range parent.Btrfs {
			if parent.Btrfs[i].Path == key {
				prec = &parent.Btrfs[i]
				break
			}
		}
		fail := ""
		switch {
		case prec == nil:
			fail = "parent snapshot carries no btrfs record for this target"
		case !prec.Complete:
			fail = "parent snapshot is not marked complete"
		case prec.SubvolUUID != info.SnapParentUUID:
			fail = "subvolume UUID changed since the parent snapshot"
		case info.SnapGeneration <= prec.Generation:
			fail = "snapshot generation did not advance"
		case prec.ConfigHash != hash:
			fail = "backup configuration changed since the parent snapshot"
		case !addSparseDirs(info, prec.Generation, key, bfs.ResolvedTarget(target), listDirs, logf):
			fail = "" // addSparseDirs already logged the reason
			sparseOK = false
		}
		if fail != "" {
			logf("btrfs sparse scan: %v, running full walk", fail)
			sparseOK = false
		}
	}
	if !sparseOK || len(analysedRoots) == 0 {
		return records, nil
	}
	// The analysed target roots are always listed.
	for _, abs := range analysedRoots {
		listDirs[abs] = true
	}
	return records, &archiver.SparsePlan{Roots: analysedRoots, ListDirs: listDirs}
}

// addSparseDirs queries the changed set of the new snapshot against
// gPrev and adds to listDirs: every changed directory at or below
// target, plus every ancestor of a changed inode down to (and
// including) the target root. Directories outside that set have no
// changed descendants and are spliced from the parent tree.
//
// The tree search speaks in physical paths inside the subvolume, so
// filtering uses physTarget (the target with any symlink resolved);
// listDirs is keyed by the spelled absolute form the archiver
// visits, so results are translated back before insertion.
func addSparseDirs(info fs.BtrfsSnapInfo, gPrev uint64, spelledAbs, physTarget string, listDirs map[string]bool, logf func(string, ...interface{})) bool {
	toSpelled := func(p string) string {
		if p == physTarget {
			return spelledAbs
		}
		if strings.HasPrefix(p, physTarget+string(os.PathSeparator)) {
			return spelledAbs + p[len(physTarget):]
		}
		return p
	}
	f, err := os.Open(info.SnapPath)
	if err != nil {
		logf("btrfs sparse scan: opening snapshot: %v, running full walk", err)
		return false
	}
	defer func() { _ = f.Close() }()

	items, err := btrfs.WalkInodeItems(f.Fd(), info.SnapID)
	if err != nil {
		logf("btrfs sparse scan: %v, running full walk", err)
		return false
	}
	// The blowup guard measures the precise changed set (D1: inode
	// item transids) only. The conservative leaf-stamp set (D2) still
	// forces listings below, but it must not veto the plan: creating
	// a snapshot restamps the leaf holding the subvolume root inode,
	// so on small trees D2 fires for every inode that shares that
	// leaf even when nothing changed. D1 stays exact regardless.
	changed := 0
	for _, it := range items {
		if it.TransID > gPrev {
			changed++
		}
	}
	if len(items) == 0 || float64(changed)/float64(len(items)) > 0.25 {
		logf("btrfs sparse scan: %d of %d inodes changed, running full walk", changed, len(items))
		return false
	}

	for ino, it := range items {
		if it.TransID <= gPrev && it.LeafStamp <= gPrev {
			continue
		}
		paths, err := btrfs.InoPaths(f.Fd(), ino)
		if err != nil {
			logf("btrfs sparse scan: %v, running full walk", err)
			return false
		}
		for _, p := range paths {
			if p == "" || p == ".." {
				continue // the subvolume root itself
			}
			abs := filepath.Join(info.SubvolRoot, filepath.FromSlash(p))
			if abs != physTarget && !strings.HasPrefix(abs, physTarget+string(os.PathSeparator)) {
				continue // belongs to a different target of this subvolume
			}
			if it.IsDir() {
				listDirs[toSpelled(abs)] = true
			}
			// Every ancestor down to the target root must be listed
			// for the walk to reach this inode at all.
			for d := filepath.Dir(abs); ; d = filepath.Dir(d) {
				listDirs[toSpelled(d)] = true
				if d == physTarget || filepath.Dir(d) == d {
					break
				}
			}
		}
	}
	return true
}
