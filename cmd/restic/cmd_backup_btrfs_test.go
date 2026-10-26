//go:build linux

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/btrfs"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/feature"
	"github.com/restic/restic/internal/fs"
)

type btrfsLogCapture struct{ lines []string }

func (l *btrfsLogCapture) logf(format string, args ...interface{}) {
	l.lines = append(l.lines, fmt.Sprintf(format, args...))
}

func (l *btrfsLogCapture) count(substr string) int {
	n := 0
	for _, s := range l.lines {
		n += strings.Count(s, substr)
	}
	return n
}

// runBtrfsPlan drives prepareBtrfsPlan with fakes for the snapshot
// wrapper. Every target reports the same snapshot, whose path is
// deliberately never created: if an analysis is attempted for a
// target, opening the snapshot fails and addSparseDirs logs it, so
// the log shows exactly which targets seeded an analysis. Parent
// records are set up so every guard would pass for every target.
func runBtrfsPlan(t *testing.T, targets []string, dirs map[string]bool) ([]data.BtrfsProvenance, *archiver.SparsePlan, *btrfsLogCapture) {
	t.Helper()
	defer feature.TestSetFlag(t, feature.Flag, feature.BtrfsSparseScan, true)()

	opts := BackupOptions{}
	info := fs.BtrfsSnapInfo{
		SubvolRoot:     "/vol",
		SnapPath:       "/vol/.restic-snap-missing",
		SourceUUID:     "uuid-1",
		SnapParentUUID: "uuid-1",
		SnapGeneration: 42,
		SnapID:         257,
	}
	parent := &data.Snapshot{}
	for _, target := range targets {
		key, err := filepath.Abs(target)
		if err != nil {
			t.Fatal(err)
		}
		parent.Btrfs = append(parent.Btrfs, data.BtrfsProvenance{
			Path:       key,
			SubvolUUID: "uuid-1",
			Generation: 41,
			Complete:   true,
			ConfigHash: btrfsConfigHash(key, opts),
		})
	}
	infoFor := func(string) (fs.BtrfsSnapInfo, bool) { return info, true }
	resolved := func(target string) string {
		abs, _ := filepath.Abs(target)
		return abs
	}
	lstat := func(name string) (*fs.ExtendedFileInfo, error) {
		if dirs[name] {
			return &fs.ExtendedFileInfo{Mode: os.ModeDir | 0o755}, nil
		}
		return &fs.ExtendedFileInfo{Mode: 0o644}, nil
	}
	logs := &btrfsLogCapture{}
	records, plan := prepareBtrfsPlan(infoFor, resolved, lstat, parent, targets, opts, logs.logf)
	return records, plan, logs
}

// A file target gets its provenance record but seeds no analysis:
// analysing its whole subvolume for it could only lose time, since a
// file is read identically in sparse and full-walk modes.
func TestPrepareBtrfsPlanFileTargetSeedsNoAnalysis(t *testing.T) {
	file := "/vol/mosquitto/mosquitto.conf"
	records, plan, logs := runBtrfsPlan(t, []string{file}, map[string]bool{file: false})

	if len(records) != 1 || records[0].Path != file {
		t.Fatalf("expected one record for the file target, got %v", records)
	}
	if plan != nil {
		t.Fatalf("expected no sparse plan for a file-only target list, got %+v", plan)
	}
	if n := logs.count("sparse scan"); n != 0 {
		t.Fatalf("expected no sparse analysis for a file target, got %d sparse log lines: %v", n, logs.lines)
	}
}

// In a mixed target list the directory target still seeds an
// analysis (exactly one attempt) and the file target does not.
func TestPrepareBtrfsPlanMixedTargetsAnalyseDirsOnly(t *testing.T) {
	file := "/vol/mosquitto/mosquitto.conf"
	dir := "/vol/grafana"
	records, plan, logs := runBtrfsPlan(t, []string{file, dir}, map[string]bool{file: false, dir: true})

	if len(records) != 2 {
		t.Fatalf("expected records for both targets, got %v", records)
	}
	// The directory target's analysis fails on the deliberately
	// missing snapshot path, which vetoes the plan.
	if plan != nil {
		t.Fatalf("expected the failed directory analysis to veto the plan, got %+v", plan)
	}
	if n := logs.count("opening snapshot"); n != 1 {
		t.Fatalf("expected exactly one analysis attempt (the directory target), got %d: %v", n, logs.lines)
	}
}

// Past the analysis budget the changed-set resolution stops
// immediately, reports the budget line, and reports failure so the
// caller falls back to a full walk.
func TestSparseListDirsBudgetAborts(t *testing.T) {
	defer func(prev int) { sparseAnalysisBudget = prev }(sparseAnalysisBudget)
	sparseAnalysisBudget = 3

	items := map[uint64]*btrfs.InodeInfo{}
	for i := uint64(1); i <= 10; i++ {
		items[1000+i] = &btrfs.InodeInfo{TransID: 50, Mode: 0o100644}
	}
	resolved := 0
	resolve := func(uint64) ([]string, error) {
		resolved++
		return []string{"sub/file"}, nil
	}
	logs := &btrfsLogCapture{}
	listDirs := map[string]bool{}
	ok := sparseListDirs(items, 10, "/vol", "/vol",
		func(p string) string { return p }, resolve, listDirs, logs.logf)

	if ok {
		t.Fatal("expected the analysis to abort past the budget")
	}
	if resolved != 3 {
		t.Fatalf("expected resolution to stop at the budget (3), got %d resolutions", resolved)
	}
	want := "btrfs sparse scan: analysis budget exceeded (4 changed inodes), running full walk"
	if len(logs.lines) != 1 || logs.lines[0] != want {
		t.Fatalf("expected the single log line %q, got %v", want, logs.lines)
	}
}

// Within the budget the changed directories and the ancestors of
// changed inodes are listed, unchanged inodes are never resolved,
// and leaf-stamp-only changes (D2) count as changed.
func TestSparseListDirsWithinBudget(t *testing.T) {
	const (
		dirIno  = 2001 // changed directory (D1)
		fileIno = 2002 // changed by leaf stamp only (D2)
		oldIno  = 2003 // unchanged
	)
	items := map[uint64]*btrfs.InodeInfo{
		dirIno:  {TransID: 50, LeafStamp: 50, Mode: 0o40755},
		fileIno: {TransID: 5, LeafStamp: 60, Mode: 0o100644},
		oldIno:  {TransID: 5, LeafStamp: 5, Mode: 0o100644},
	}
	paths := map[uint64][]string{
		dirIno:  {"sub"},
		fileIno: {"sub/file"},
		oldIno:  {"sub/old"},
	}
	var resolved []uint64
	resolve := func(ino uint64) ([]string, error) {
		resolved = append(resolved, ino)
		return paths[ino], nil
	}
	logs := &btrfsLogCapture{}
	listDirs := map[string]bool{}
	ok := sparseListDirs(items, 10, "/vol", "/vol",
		func(p string) string { return p }, resolve, listDirs, logs.logf)

	if !ok {
		t.Fatalf("expected the analysis to succeed within the budget, logs: %v", logs.lines)
	}
	if len(resolved) != 2 {
		t.Fatalf("expected exactly the two changed inodes to be resolved, got %v", resolved)
	}
	want := map[string]bool{"/vol": true, "/vol/sub": true}
	if !reflect.DeepEqual(listDirs, want) {
		t.Fatalf("expected listDirs %v, got %v", want, listDirs)
	}
}
