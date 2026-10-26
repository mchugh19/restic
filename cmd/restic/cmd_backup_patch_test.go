package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/feature"
	"github.com/restic/restic/internal/fs"
	"github.com/restic/restic/internal/global"
	"github.com/restic/restic/internal/restic"
	rtest "github.com/restic/restic/internal/test"
)

func writeListFile(t testing.TB, dir, name string, lines []string) string {
	t.Helper()
	path := filepath.Join(dir, name)
	content := strings.Join(lines, "\n")
	if len(lines) > 0 {
		content += "\n"
	}
	rtest.OK(t, os.WriteFile(path, []byte(content), 0o644))
	return path
}

func TestLoadPatchLists(t *testing.T) {
	dir := t.TempDir()
	changed := writeListFile(t, dir, "changed", []string{"/t/a", "/t/d/f", "/t//double", "/t/./dot"})
	deleted := writeListFile(t, dir, "deleted", []string{"/t/old"})
	lists, err := loadPatchLists(changed, deleted)
	rtest.OK(t, err)
	rtest.Equals(t, []string{"/t/a", "/t/d/f", "/t/double", "/t/dot"}, lists.Changed)
	rtest.Equals(t, []string{"/t/old"}, lists.Deleted)

	// empty files are valid (a run with no changes)
	emptyC := writeListFile(t, dir, "emptyc", nil)
	emptyD := writeListFile(t, dir, "emptyd", nil)
	lists, err = loadPatchLists(emptyC, emptyD)
	rtest.OK(t, err)
	rtest.Equals(t, 0, len(lists.Changed))
	rtest.Equals(t, 0, len(lists.Deleted))

	// an unreadable list is an error (the caller falls back to a
	// full walk)
	_, err = loadPatchLists(filepath.Join(dir, "missing"), deleted)
	rtest.Assert(t, err != nil, "missing list file did not error")
}

func TestLoadPatchListsMalformed(t *testing.T) {
	dir := t.TempDir()
	good := writeListFile(t, dir, "good", []string{"/t/a"})
	for name, lines := range map[string][]string{
		"blank line":      {"/t/a", "", "/t/b"},
		"relative path":   {"t/a"},
		"duplicate":       {"/t/a", "/t/a"},
		"dup after clean": {"/t/a", "/t//a"},
	} {
		t.Run(name, func(t *testing.T) {
			bad := writeListFile(t, dir, "bad", lines)
			_, err := loadPatchLists(bad, good)
			rtest.Assert(t, err != nil, "malformed list %v did not error", lines)
			_, err = loadPatchLists(good, bad)
			rtest.Assert(t, err != nil, "malformed deleted list %v did not error", lines)
		})
	}
	// a path in both lists violates the disjointness contract
	both := writeListFile(t, dir, "both", []string{"/t/shared"})
	shared := writeListFile(t, dir, "shared", []string{"/t/shared"})
	_, err := loadPatchLists(both, shared)
	rtest.Assert(t, err != nil, "path in both lists did not error")
}

// patchPlanFakes provides the filesystem and parent tree of a small
// fixture rooted at /t:
//
//	/t/keep.txt        file, in parent
//	/t/mod.txt         file, in parent
//	/t/d/f.txt         file, in parent
//	/t/dirfile         regular file, in parent (an ancestor trap)
//	/t/linkdir         symlink, with a file beneath it
//	/t/newdir/         directory, not in parent
//	/t/newdir/deep.txt file, not in parent
type patchPlanFakes struct {
	files map[string]*fs.ExtendedFileInfo
	nodes map[string]*data.Node
}

func newPatchPlanFakes() *patchPlanFakes {
	f := &patchPlanFakes{
		files: map[string]*fs.ExtendedFileInfo{},
		nodes: map[string]*data.Node{},
	}
	file := func(p string) { f.files[p] = &fs.ExtendedFileInfo{Mode: 0o644} }
	dir := func(p string) { f.files[p] = &fs.ExtendedFileInfo{Mode: os.ModeDir | 0o755} }
	for _, p := range []string{"/t/keep.txt", "/t/mod.txt", "/t/d/f.txt", "/t/dirfile", "/t/newdir/deep.txt", "/t/linkdir/f.txt"} {
		file(p)
	}
	for _, p := range []string{"/t", "/t/d", "/t/newdir"} {
		dir(p)
	}
	f.files["/t/linkdir"] = &fs.ExtendedFileInfo{Mode: os.ModeSymlink | 0o777}

	pfile := func(p string) { f.nodes[p] = &data.Node{Type: data.NodeTypeFile} }
	pdir := func(p string) { f.nodes[p] = &data.Node{Type: data.NodeTypeDir} }
	for _, p := range []string{"/t/keep.txt", "/t/mod.txt", "/t/d/f.txt", "/t/dirfile"} {
		pfile(p)
	}
	pfile("/t/gone.txt") // in the parent only: deleted since
	for _, p := range []string{"/t", "/t/d"} {
		pdir(p)
	}
	return f
}

func (f *patchPlanFakes) lstat(p string) (*fs.ExtendedFileInfo, error) {
	if fi, ok := f.files[p]; ok {
		return fi, nil
	}
	return nil, fmt.Errorf("lstat %v: no such file", p)
}

func (f *patchPlanFakes) parentNode(p string) (*data.Node, error) {
	if n, ok := f.nodes[p]; ok {
		return n, nil
	}
	return nil, nil
}

type patchLogCapture struct{ lines []string }

func (l *patchLogCapture) logf(format string, args ...interface{}) {
	l.lines = append(l.lines, fmt.Sprintf(format, args...))
}

func (l *patchLogCapture) joined() string { return strings.Join(l.lines, "\n") }

func planFor(f *patchPlanFakes, lists *patchLists) (*archiver.PatchPlan, *patchLogCapture) {
	treeID := restic.ID{}
	parent := &data.Snapshot{Tree: &treeID}
	logs := &patchLogCapture{}
	plan := buildPatchPlan(f.lstat, f.parentNode, parent, lists, []string{"/t"}, logs.logf)
	return plan, logs
}

func TestBuildPatchPlanValid(t *testing.T) {
	f := newPatchPlanFakes()
	plan, _ := planFor(f, &patchLists{
		Changed: []string{"/t/mod.txt", "/t/newdir", "/t/newdir/deep.txt"},
		Deleted: []string{"/t/gone.txt"},
	})
	rtest.Assert(t, plan != nil, "valid lists produced no plan")
	// the root, the changed directory itself and the ancestor of
	// the deeply changed file are listed; /t/d is not (nothing
	// below it is listed)
	rtest.Equals(t, map[string]bool{"/t": true, "/t/newdir": true}, plan.ListDirs)
	rtest.Equals(t, map[string]bool{
		"/t/mod.txt": true, "/t/newdir": true, "/t/newdir/deep.txt": true,
	}, plan.Changed)
	rtest.Equals(t, map[string]bool{"/t/gone.txt": true}, plan.Deleted)
	rtest.Equals(t, []string{"/t"}, plan.Roots)

	// empty lists: everything below the roots is spliced
	plan, _ = planFor(f, &patchLists{})
	rtest.Assert(t, plan != nil, "empty lists produced no plan")
	rtest.Equals(t, map[string]bool{"/t": true}, plan.ListDirs)

	// a changed entry naming the target root itself is a no-op
	plan, _ = planFor(f, &patchLists{Changed: []string{"/t"}})
	rtest.Assert(t, plan != nil, "root-as-changed produced no plan")
	rtest.Equals(t, 0, len(plan.Changed))
}

func TestBuildPatchPlanAbort(t *testing.T) {
	cases := []struct {
		name    string
		lists   *patchLists
		wantLog string
	}{
		{"changed missing", &patchLists{Changed: []string{"/t/nope.txt"}}, "cannot be stated"},
		{"changed outside targets", &patchLists{Changed: []string{"/other/f"}}, "outside the backup targets"},
		{"changed below a file", &patchLists{Changed: []string{"/t/dirfile/x"}}, "cannot be stated"},
		{"changed below a symlink", &patchLists{Changed: []string{"/t/linkdir/f.txt"}}, "is not a directory"},
		{"deleted not in parent", &patchLists{Deleted: []string{"/t/newdir/deep.txt"}}, "is not in the parent snapshot"},
		{"deleted still exists", &patchLists{Deleted: []string{"/t/mod.txt"}}, "still exists"},
		{"deleted is target root", &patchLists{Deleted: []string{"/t"}}, "is a backup target"},
		{"deleted outside targets", &patchLists{Deleted: []string{"/other/f"}}, "outside the backup targets"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			f := newPatchPlanFakes()
			plan, logs := planFor(f, tc.lists)
			rtest.Assert(t, plan == nil, "expected no plan, got %v", plan)
			rtest.Assert(t, strings.Contains(logs.joined(), tc.wantLog),
				"log %q does not contain %q", logs.joined(), tc.wantLog)
		})
	}

	// no parent at all
	f := newPatchPlanFakes()
	logs := &patchLogCapture{}
	plan := buildPatchPlan(f.lstat, f.parentNode, nil, &patchLists{}, []string{"/t"}, logs.logf)
	rtest.Assert(t, plan == nil, "nil parent produced a plan")
	rtest.Assert(t, strings.Contains(logs.joined(), "no parent snapshot"), "log: %q", logs.joined())

	// a deleted path whose parent-chain ancestor is a file in the
	// parent tree: /t/dirfile/child does not resolve in the parent
	// (the fake returns nil), so this aborts as "not in the parent
	// snapshot" before the ancestor check is even reached.
	plan, logs = planFor(f, &patchLists{Deleted: []string{"/t/dirfile/child"}})
	rtest.Assert(t, plan == nil, "expected no plan")
	rtest.Assert(t, strings.Contains(logs.joined(), "not in the parent snapshot"), "log: %q", logs.joined())
}

// patchStep is one mutation of the fixture tree plus the lists a
// producer would report for it (paths relative to the fixture root).
type patchStep struct {
	name    string
	apply   func(t testing.TB, src string)
	changed []string
	deleted []string
}

func patchJoin(src string, rels []string) []string {
	out := make([]string, 0, len(rels))
	for _, r := range rels {
		out = append(out, filepath.Join(src, filepath.FromSlash(r)))
	}
	return out
}

func lsJSONNodeLines(t testing.TB, gopts global.Options, snapshotID, target string) []string {
	t.Helper()
	gopts.JSON = true
	out := testRunLsWithOpts(t, gopts, LsOptions{Recursive: true}, []string{snapshotID})
	lines := strings.Split(strings.TrimSuffix(string(out), "\n"), "\n")
	rtest.Assert(t, len(lines) > 1, "ls output too short: %v", lines)
	// The first line describes the snapshot itself; the rest are its
	// nodes, including every directory's subtree ID and every file's
	// content list, so stream equality at or below the target is
	// tree identity. Nodes above the target are structural path
	// components outside the fixture and are filtered out: their
	// metadata is not part of the comparison.
	//
	// Two fields are normalised away: ctime and atime. On this
	// sandbox's filesystem a file's ctime is materialised lazily —
	// a later stat of an untouched file can return a newer ctime
	// than an earlier stat did — so even two full walks of an
	// unchanged tree flake on ctime here (the malformed-list steps
	// below, which compare full walk against full walk, showed
	// exactly that). The testbed gate compares tree blob IDs on a
	// real filesystem, where ctime is a stored, stable value, and
	// loses nothing. Everything else — names, types, sizes, modes,
	// mtimes, inodes, subtree IDs and content lists — is compared
	// exactly.
	nodes := make([]string, 0, len(lines))
	for _, line := range lines[1:] {
		var node map[string]any
		rtest.OK(t, json.Unmarshal([]byte(line), &node))
		path, _ := node["path"].(string)
		if path != target && !strings.HasPrefix(path, target+string(os.PathSeparator)) {
			continue
		}
		delete(node, "ctime")
		delete(node, "atime")
		canonical, err := json.Marshal(node)
		rtest.OK(t, err)
		nodes = append(nodes, string(canonical))
	}
	rtest.Assert(t, len(nodes) > 0, "no nodes at or below %v in ls output", target)
	return nodes
}

// TestBackupPatchMode runs the hand-written half of the project 6
// tree-identity gate: for each mutation class, a patch-mode backup
// driven by hand-written lists must produce exactly the tree a
// --force full backup produces over the same filesystem state.
func TestBackupPatchMode(t *testing.T) {
	defer feature.TestSetFlag(t, feature.Flag, feature.PatchMode, true)()
	env, cleanup := withTestEnvironment(t)
	defer cleanup()
	testRunInit(t, env.gopts)

	src := filepath.Join(t.TempDir(), "src")
	write := func(rel, content string) {
		p := filepath.Join(src, filepath.FromSlash(rel))
		rtest.OK(t, os.MkdirAll(filepath.Dir(p), 0o755))
		rtest.OK(t, os.WriteFile(p, []byte(content), 0o644))
	}
	write("keep.txt", "keep")
	write("keep2.txt", "same-size")
	write("mod.txt", "before")
	write("del.txt", "delete me")
	write("orig.txt", "hardlink target")
	write("typed.txt", "will become a directory")
	write("sub/inner.txt", "inner")
	write("sub/gone.txt", "gone")
	write("other/place.txt", "place")
	write("deltree/x.txt", "x")
	write("deltree/sub/y.txt", "y")
	write("typedir/child.txt", "child")
	write("links/placeholder.txt", "placeholder")
	rtest.OK(t, os.Symlink("keep.txt", filepath.Join(src, "link.txt")))

	known := map[string]bool{}
	snapshotCount := 0
	newSnapshotID := func() string {
		t.Helper()
		snapshotCount++
		ids := testListSnapshots(t, env.gopts, snapshotCount)
		for _, id := range ids {
			if !known[id.String()] {
				known[id.String()] = true
				return id.String()
			}
		}
		t.Fatal("no new snapshot appeared")
		return ""
	}

	testRunBackup(t, "", []string{src}, BackupOptions{}, env.gopts)
	lastGood := newSnapshotID()

	steps := []patchStep{
		{"modify", func(t testing.TB, src string) {
			write("mod.txt", "after")
		}, []string{"mod.txt"}, nil},
		{"create", func(t testing.TB, src string) {
			write("created.txt", "created")
		}, []string{"created.txt"}, nil},
		{"delete file", func(t testing.TB, src string) {
			rtest.OK(t, os.Remove(filepath.Join(src, "del.txt")))
		}, nil, []string{"del.txt"}},
		{"delete tree", func(t testing.TB, src string) {
			rtest.OK(t, os.RemoveAll(filepath.Join(src, "deltree")))
		}, nil, []string{"deltree", "deltree/sub", "deltree/sub/y.txt", "deltree/x.txt"}},
		{"rename within", func(t testing.TB, src string) {
			rtest.OK(t, os.Rename(filepath.Join(src, "sub", "inner.txt"), filepath.Join(src, "sub", "inner2.txt")))
		}, []string{"sub/inner2.txt"}, []string{"sub/inner.txt"}},
		{"rename across", func(t testing.TB, src string) {
			rtest.OK(t, os.Rename(filepath.Join(src, "sub", "gone.txt"), filepath.Join(src, "other", "gone.txt")))
		}, []string{"other/gone.txt"}, []string{"sub/gone.txt"}},
		{"rename out of target", func(t testing.TB, src string) {
			rtest.OK(t, os.Rename(filepath.Join(src, "other", "place.txt"), filepath.Join(filepath.Dir(src), "escaped.txt")))
		}, nil, []string{"other/place.txt"}},
		{"rename into target", func(t testing.TB, src string) {
			rtest.OK(t, os.Rename(filepath.Join(filepath.Dir(src), "escaped.txt"), filepath.Join(src, "other", "back.txt")))
		}, []string{"other/back.txt"}, nil},
		{"hardlink add", func(t testing.TB, src string) {
			rtest.OK(t, os.Link(filepath.Join(src, "orig.txt"), filepath.Join(src, "links", "added.txt")))
		}, []string{"links/added.txt", "orig.txt"}, nil},
		{"hardlink remove", func(t testing.TB, src string) {
			rtest.OK(t, os.Remove(filepath.Join(src, "links", "added.txt")))
		}, []string{"orig.txt"}, []string{"links/added.txt"}},
		{"chmod only", func(t testing.TB, src string) {
			rtest.OK(t, os.Chmod(filepath.Join(src, "keep.txt"), 0o600))
		}, []string{"keep.txt"}, nil},
		{"utimes only", func(t testing.TB, src string) {
			when := time.Date(2021, 3, 4, 5, 6, 7, 0, time.UTC)
			rtest.OK(t, os.Chtimes(filepath.Join(src, "mod.txt"), when, when))
		}, []string{"mod.txt"}, nil},
		{"symlink retarget", func(t testing.TB, src string) {
			rtest.OK(t, os.Remove(filepath.Join(src, "link.txt")))
			rtest.OK(t, os.Symlink("mod.txt", filepath.Join(src, "link.txt")))
		}, []string{"link.txt"}, nil},
		{"nested create", func(t testing.TB, src string) {
			write("deep/top.txt", "top")
			write("deep/a/b/c.txt", "deep")
		}, []string{"deep", "deep/top.txt", "deep/a", "deep/a/b", "deep/a/b/c.txt"}, nil},
		{"type change file to dir", func(t testing.TB, src string) {
			rtest.OK(t, os.Remove(filepath.Join(src, "typed.txt")))
			write("typed.txt/child.txt", "child")
		}, []string{"typed.txt", "typed.txt/child.txt"}, nil},
		{"type change dir to file", func(t testing.TB, src string) {
			rtest.OK(t, os.RemoveAll(filepath.Join(src, "typedir")))
			write("typedir", "now a file")
		}, []string{"typedir"}, []string{"typedir/child.txt"}},
		{"rewrite preserving mtime and size", func(t testing.TB, src string) {
			p := filepath.Join(src, "keep2.txt")
			fi, err := os.Stat(p)
			rtest.OK(t, err)
			rtest.OK(t, os.WriteFile(p, []byte("same_size"), 0o644))
			rtest.OK(t, os.Chtimes(p, fi.ModTime(), fi.ModTime()))
		}, []string{"keep2.txt"}, nil},
	}

	listDir := t.TempDir()
	for _, step := range steps {
		t.Run(step.name, func(t *testing.T) {
			step.apply(t, src)
			changedFile := writeListFile(t, listDir, "changed", patchJoin(src, step.changed))
			deletedFile := writeListFile(t, listDir, "deleted", patchJoin(src, step.deleted))
			testRunBackup(t, "", []string{src}, BackupOptions{
				PatchChangedFrom: changedFile,
				PatchDeletedFrom: deletedFile,
			}, env.gopts)
			patchID := newSnapshotID()

			testRunBackup(t, "", []string{src}, BackupOptions{Force: true}, env.gopts)
			forceID := newSnapshotID()

			rtest.Equals(t,
				lsJSONNodeLines(t, env.gopts, forceID, src),
				lsJSONNodeLines(t, env.gopts, patchID, src))
			lastGood = forceID
		})
	}

	// Malformed lists must degrade to a full walk and still produce
	// the correct tree (identical to the last reference snapshot,
	// as the tree does not change during these steps).
	malformed := []struct {
		name    string
		changed []string
		deleted []string
	}{
		{"deleted path not in parent", nil, []string{"never-existed.txt"}},
		{"changed path not on disk", []string{"ghost.txt"}, nil},
		{"duplicate entries", []string{"keep.txt", "keep.txt"}, nil},
		{"path in both lists", []string{"keep.txt"}, []string{"keep.txt"}},
		{"changed path below a file", []string{"orig.txt/child"}, nil},
	}
	for _, step := range malformed {
		t.Run("malformed: "+step.name, func(t *testing.T) {
			changedFile := writeListFile(t, listDir, "changed", patchJoin(src, step.changed))
			deletedFile := writeListFile(t, listDir, "deleted", patchJoin(src, step.deleted))
			testRunBackup(t, "", []string{src}, BackupOptions{
				PatchChangedFrom: changedFile,
				PatchDeletedFrom: deletedFile,
			}, env.gopts)
			id := newSnapshotID()
			rtest.Equals(t,
				lsJSONNodeLines(t, env.gopts, lastGood, src),
				lsJSONNodeLines(t, env.gopts, id, src))
		})
	}

	// Empty lists: the whole tree is inherited from the parent.
	t.Run("empty lists", func(t *testing.T) {
		changedFile := writeListFile(t, listDir, "changed", nil)
		deletedFile := writeListFile(t, listDir, "deleted", nil)
		testRunBackup(t, "", []string{src}, BackupOptions{
			PatchChangedFrom: changedFile,
			PatchDeletedFrom: deletedFile,
		}, env.gopts)
		id := newSnapshotID()
		rtest.Equals(t, lsJSONNodeLines(t, env.gopts, lastGood, src), lsJSONNodeLines(t, env.gopts, id, src))
	})

	testRunCheck(t, env.gopts)
}
