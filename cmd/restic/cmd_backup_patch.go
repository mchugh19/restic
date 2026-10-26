package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/feature"
	"github.com/restic/restic/internal/fs"
	"github.com/restic/restic/internal/restic"
)

// Patch mode (project 6) builds a snapshot by patching the parent
// snapshot's tree: an external producer supplies the paths that
// changed and the paths that were deleted since the parent, and the
// walk only lists the directories needed to reach those paths. The
// list format is deliberately minimal so lists stay diffable,
// replayable and generatable by hand:
//
//   - Two files, one for changed paths (--patch-changed-from) and
//     one for deleted paths (--patch-deleted-from).
//   - One path per line, LF-terminated, UTF-8. A single trailing
//     newline at end of file is the terminator, not an entry.
//   - Paths are absolute and name the path exactly as the backup
//     records it (the spelled target path, before any snapshot
//     translation by the filesystem wrapper).
//   - No blank lines, no duplicates within a file or across both
//     files. A path deleted and recreated is a change, not a
//     deletion: the lists are disjoint by contract.
//   - File names containing a newline cannot be represented; a
//     producer that meets one must decline to produce lists (the
//     wrapper then runs a plain backup).
//
// Patch mode is fail-safe: any input that cannot be reconciled
// against the filesystem and the parent tree discards the plan and
// the backup runs as a full walk, with the reason logged. It never
// produces a partial tree presented as complete.

// patchLists holds the parsed contents of the two list files.
type patchLists struct {
	Changed []string
	Deleted []string
}

// readPatchListFile parses one list file. It returns the cleaned
// absolute paths in file order.
func readPatchListFile(name string) ([]string, error) {
	raw, err := os.ReadFile(name)
	if err != nil {
		return nil, err
	}
	text := string(raw)
	if text == "" {
		return nil, nil
	}
	lines := strings.Split(text, "\n")
	// A final newline terminates the last entry rather than opening
	// an empty one.
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	paths := make([]string, 0, len(lines))
	for i, line := range lines {
		if line == "" {
			return nil, fmt.Errorf("%v:%d: blank line", name, i+1)
		}
		if !filepath.IsAbs(line) {
			return nil, fmt.Errorf("%v:%d: path %q is not absolute", name, i+1, line)
		}
		paths = append(paths, filepath.Clean(line))
	}
	return paths, nil
}

// loadPatchLists reads both list files and checks the pair for
// duplicates, within each file and across both.
func loadPatchLists(changedFile, deletedFile string) (*patchLists, error) {
	changed, err := readPatchListFile(changedFile)
	if err != nil {
		return nil, fmt.Errorf("reading changed list: %w", err)
	}
	deleted, err := readPatchListFile(deletedFile)
	if err != nil {
		return nil, fmt.Errorf("reading deleted list: %w", err)
	}
	seen := make(map[string]bool, len(changed)+len(deleted))
	for _, p := range changed {
		if seen[p] {
			return nil, fmt.Errorf("path %q appears twice in the changed list", p)
		}
		seen[p] = true
	}
	for _, p := range deleted {
		if seen[p] {
			if contains(changed, p) {
				return nil, fmt.Errorf("path %q appears in both the changed and the deleted list", p)
			}
			return nil, fmt.Errorf("path %q appears twice in the deleted list", p)
		}
		seen[p] = true
	}
	return &patchLists{Changed: changed, Deleted: deleted}, nil
}

// parentTreeNode returns the node for an absolute path in the
// snapshot's tree, or nil if the path is not present. The snapshot
// tree mirrors the filesystem layout from the root, so the lookup
// walks one tree level per path component.
func parentTreeNode(ctx context.Context, loader restic.BlobLoader, sn *data.Snapshot, path string) (*data.Node, error) {
	if sn == nil || sn.Tree == nil {
		return nil, nil
	}
	rel := strings.TrimPrefix(path, string(filepath.Separator))
	if rel == "" {
		return nil, nil // the root itself has no node
	}
	treeID := *sn.Tree
	var node *data.Node
	comps := strings.Split(rel, string(filepath.Separator))
	for i, comp := range comps {
		it, err := data.LoadTree(ctx, loader, treeID)
		if err != nil {
			return nil, err
		}
		node = nil
		for noe := range it {
			if noe.Error != nil {
				return nil, noe.Error
			}
			if noe.Node.Name == comp {
				node = noe.Node
				break
			}
			if noe.Node.Name > comp {
				break // nodes are sorted; the component is absent
			}
		}
		if node == nil {
			return nil, nil
		}
		if node.Subtree == nil {
			if i != len(comps)-1 {
				// A leaf before the final component: the path
				// continues below a non-directory and is absent.
				return nil, nil
			}
			return node, nil
		}
		treeID = *node.Subtree
	}
	return node, nil
}

func contains(paths []string, p string) bool {
	for _, q := range paths {
		if q == p {
			return true
		}
	}
	return false
}

// preparePatchRun validates the requested patch lists and, when
// everything reconciles, returns the plan for the archiver. Any
// problem leaves the plan nil (a full walk) and is reported through
// logf; falling back is always safe, so this never fails the backup.
func preparePatchRun(ctx context.Context, loader restic.BlobLoader, targetFS fs.FS, parent *data.Snapshot, targets []string, opts BackupOptions, logf func(string, ...interface{})) *archiver.PatchPlan {
	if !feature.Flag.Enabled(feature.PatchMode) {
		logf("patch mode: lists given but the patch-mode feature is disabled, running full walk")
		return nil
	}
	if opts.PatchChangedFrom == "" || opts.PatchDeletedFrom == "" {
		logf("patch mode: --patch-changed-from and --patch-deleted-from must be given together, running full walk")
		return nil
	}
	lists, err := loadPatchLists(opts.PatchChangedFrom, opts.PatchDeletedFrom)
	if err != nil {
		logf("patch mode: %v, running full walk", err)
		return nil
	}
	parentNode := func(path string) (*data.Node, error) {
		return parentTreeNode(ctx, loader, parent, path)
	}
	return buildPatchPlan(targetFS.Lstat, parentNode, parent, lists, targets, logf)
}

// buildPatchPlan is preparePatchRun against the filesystem and
// parent-tree lookups it needs, taken as functions so tests can
// substitute fakes. It returns nil (and logs the reason) whenever an
// entry cannot be reconciled.
func buildPatchPlan(
	lstat func(string) (*fs.ExtendedFileInfo, error),
	parentNode func(string) (*data.Node, error),
	parent *data.Snapshot, lists *patchLists, targets []string,
	logf func(string, ...interface{}),
) *archiver.PatchPlan {
	if parent == nil {
		logf("patch mode: no parent snapshot, running full walk")
		return nil
	}
	if parent.Tree == nil {
		logf("patch mode: parent snapshot has no tree, running full walk")
		return nil
	}

	absTargets := make([]string, 0, len(targets))
	seenTarget := make(map[string]bool)
	for _, target := range targets {
		abs, err := filepath.Abs(target)
		if err != nil {
			abs = filepath.Clean(target)
		}
		if !seenTarget[abs] {
			seenTarget[abs] = true
			absTargets = append(absTargets, abs)
		}
	}
	// rootOf returns the target root containing p, or "".
	rootOf := func(p string) string {
		for _, root := range absTargets {
			if p == root || strings.HasPrefix(p, root+string(os.PathSeparator)) {
				return root
			}
		}
		return ""
	}

	listDirs := make(map[string]bool)
	for _, root := range absTargets {
		listDirs[root] = true
	}
	// addAncestors records every ancestor of p below (and excluding)
	// its target root as a directory to list, innermost first.
	addAncestors := func(p, root string) {
		for d := filepath.Dir(p); ; d = filepath.Dir(d) {
			listDirs[d] = true
			if d == root || filepath.Dir(d) == d {
				break
			}
		}
	}

	changed := make(map[string]bool, len(lists.Changed))
	for _, p := range lists.Changed {
		root := rootOf(p)
		if root == "" {
			logf("patch mode: changed path %v is outside the backup targets, running full walk", p)
			return nil
		}
		if p == root {
			continue // a target root is always walked; nothing to reconcile
		}
		fi, err := lstat(p)
		if err != nil {
			logf("patch mode: changed path %v cannot be stated (%v), running full walk", p, err)
			return nil
		}
		// Every ancestor between the root and p must be a real
		// directory in the filesystem: the walk reaches p through
		// them, and a file or symlink in the chain would silently
		// cut the walk off above p.
		for d := filepath.Dir(p); d != root; d = filepath.Dir(d) {
			dfi, err := lstat(d)
			if err != nil {
				logf("patch mode: parent directory %v of changed path %v cannot be stated (%v), running full walk", d, p, err)
				return nil
			}
			if !dfi.Mode.IsDir() {
				logf("patch mode: parent directory %v of changed path %v is not a directory, running full walk", d, p)
				return nil
			}
		}
		if fi.Mode.IsDir() {
			listDirs[p] = true
		}
		addAncestors(p, root)
		changed[p] = true
	}

	deleted := make(map[string]bool, len(lists.Deleted))
	for _, p := range lists.Deleted {
		root := rootOf(p)
		if root == "" {
			logf("patch mode: deleted path %v is outside the backup targets, running full walk", p)
			return nil
		}
		if p == root {
			logf("patch mode: deleted path %v is a backup target, running full walk", p)
			return nil
		}
		node, err := parentNode(p)
		if err != nil {
			logf("patch mode: reading the parent tree for deleted path %v failed (%v), running full walk", p, err)
			return nil
		}
		if node == nil {
			logf("patch mode: deleted path %v is not in the parent snapshot, running full walk", p)
			return nil
		}
		// The ancestor chain must exist as directories in the
		// parent tree: the walk drops p while listing them.
		for d := filepath.Dir(p); d != root; d = filepath.Dir(d) {
			dn, err := parentNode(d)
			if err != nil {
				logf("patch mode: reading the parent tree for directory %v failed (%v), running full walk", d, err)
				return nil
			}
			if dn == nil || dn.Type != data.NodeTypeDir {
				logf("patch mode: parent directory %v of deleted path %v is not a directory in the parent snapshot, running full walk", d, p)
				return nil
			}
		}
		if _, err := lstat(p); err == nil {
			logf("patch mode: deleted path %v still exists, running full walk", p)
			return nil
		}
		addAncestors(p, root)
		deleted[p] = true
	}

	return &archiver.PatchPlan{
		Roots:    absTargets,
		ListDirs: listDirs,
		Changed:  changed,
		Deleted:  deleted,
	}
}
