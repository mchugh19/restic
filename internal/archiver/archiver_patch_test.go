package archiver

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/fs"
	rtest "github.com/restic/restic/internal/test"
)

// findNode walks the snapshot tree along the components of target
// (an absolute path) and returns the node stored for it.
func findNode(t testing.TB, repo archiverRepo, sn *data.Snapshot, target string) *data.Node {
	t.Helper()
	ctx := context.TODO()
	rel := strings.TrimPrefix(filepath.ToSlash(target), "/")
	treeID := *sn.Tree
	var node *data.Node
	for _, comp := range strings.Split(rel, "/") {
		it, err := data.LoadTree(ctx, repo, treeID)
		rtest.OK(t, err)
		node = nil
		for noe := range it {
			rtest.OK(t, noe.Error)
			if noe.Node.Name == comp {
				node = noe.Node
				break
			}
		}
		rtest.Assert(t, node != nil, "component %v of %v not found in tree", comp, target)
		if node.Subtree == nil {
			return node
		}
		treeID = *node.Subtree
	}
	return node
}

// TestArchiverPatchMode checks the archiver half of project 6: with a
// PatchPlan, the walk lists only the planned directories, splices the
// rest from the parent tree, and produces exactly the tree a full
// walk produces over the same filesystem state.
func TestArchiverPatchMode(t *testing.T) {
	ctx := context.TODO()
	tempdir, repo := prepareTempdirRepoSrc(t, TestDir{
		"keep.txt": TestFile{Content: "keep"},
		"mod.txt":  TestFile{Content: "before"},
		"del.txt":  TestFile{Content: "delete me"},
		"orig.txt": TestFile{Content: "hardlink target"},
		"sub": TestDir{
			"inner.txt": TestFile{Content: "inner"},
			"gone.txt":  TestFile{Content: "gone"},
		},
		"links": TestDir{
			"placeholder.txt": TestFile{Content: "placeholder"},
		},
		"splice": TestDir{
			"a.txt": TestFile{Content: "aaa"},
			"b.txt": TestFile{Content: "bbb"},
		},
	})
	target := tempdir
	arch := New(repo, fs.NewLocal(), Options{})

	sn1, _, _, err := arch.Snapshot(ctx, []string{target}, SnapshotOptions{Time: time.Now()})
	rtest.OK(t, err)

	// Mutate the tree. The lists below name every mutation, the way
	// the send producer (and the ground-truth differ) report them:
	// a hardlink add names every path of the inode, because the
	// pre-existing path's link count changes.
	write := func(name, content string) {
		rtest.OK(t, os.WriteFile(filepath.Join(target, name), []byte(content), 0o644))
	}
	write("mod.txt", "after")
	write("new.txt", "created")
	rtest.OK(t, os.Remove(filepath.Join(target, "del.txt")))
	rtest.OK(t, os.Rename(filepath.Join(target, "sub", "inner.txt"), filepath.Join(target, "sub", "renamed.txt")))
	rtest.OK(t, os.Remove(filepath.Join(target, "sub", "gone.txt")))
	rtest.OK(t, os.Link(filepath.Join(target, "orig.txt"), filepath.Join(target, "links", "added.txt")))
	rtest.OK(t, os.Chmod(filepath.Join(target, "keep.txt"), 0o600))

	join := func(elem ...string) string {
		return filepath.Join(append([]string{target}, elem...)...)
	}
	listDirs := map[string]bool{target: true, join("sub"): true, join("links"): true}
	plan := &PatchPlan{
		Roots:    []string{target},
		ListDirs: listDirs,
		Changed: map[string]bool{
			join("mod.txt"): true, join("new.txt"): true,
			join("sub", "renamed.txt"): true,
			join("orig.txt"):           true, join("links", "added.txt"): true,
			join("keep.txt"): true,
		},
		Deleted: map[string]bool{
			join("del.txt"): true, join("sub", "inner.txt"): true, join("sub", "gone.txt"): true,
		},
	}

	sn2, _, _, err := arch.Snapshot(ctx, []string{target}, SnapshotOptions{
		Time:           time.Now(),
		ParentSnapshot: sn1,
		PatchPlan:      plan,
	})
	rtest.OK(t, err)

	// Reference: a full walk over the same state, same parent.
	sn3, _, _, err := arch.Snapshot(ctx, []string{target}, SnapshotOptions{
		Time:           time.Now(),
		ParentSnapshot: sn1,
	})
	rtest.OK(t, err)

	// Compare at the target node rather than the snapshot root: the
	// structural ancestors above the target live outside the
	// fixture, and their metadata is not part of the contract (on
	// this sandbox's filesystem the mount root's timestamps are
	// synthesised per stat and differ between any two runs, even
	// full against full). Node equality includes the subtree ID,
	// so this asserts the whole tree below the target is identical.
	node2 := findNode(t, repo, sn2, target)
	node3 := findNode(t, repo, sn3, target)
	rtest.Assert(t, node2.Equals(*node3),
		"patch-mode tree below %v differs from the full-walk tree", target)

	// The untouched "splice" subtree must be the parent's blob,
	// reused rather than rebuilt.
	splice1 := findNode(t, repo, sn1, join("splice"))
	splice2 := findNode(t, repo, sn2, join("splice"))
	rtest.Assert(t, splice1.Subtree.Equal(*splice2.Subtree),
		"spliced subtree was rebuilt: %v vs %v", splice1.Subtree, splice2.Subtree)

	// Poison check: change a file inside the spliced subtree without
	// listing it. The patch walk must keep the parent's version (it
	// never looks), while a full walk picks the change up. This pins
	// the contract that makes the plan builder's reconciliation and
	// ancestor closure load-bearing: the archiver trusts the plan.
	write("splice/a.txt", "poisoned")
	sn4, _, _, err := arch.Snapshot(ctx, []string{target}, SnapshotOptions{
		Time:           time.Now(),
		ParentSnapshot: sn2,
		PatchPlan:      &PatchPlan{Roots: plan.Roots, ListDirs: map[string]bool{target: true}},
	})
	rtest.OK(t, err)
	splice4 := findNode(t, repo, sn4, join("splice"))
	rtest.Assert(t, splice2.Subtree.Equal(*splice4.Subtree),
		"unlisted change leaked into the patch snapshot: %v vs %v", splice2.Subtree, splice4.Subtree)

	sn5, _, _, err := arch.Snapshot(ctx, []string{target}, SnapshotOptions{
		Time:           time.Now(),
		ParentSnapshot: sn2,
	})
	rtest.OK(t, err)
	splice5 := findNode(t, repo, sn5, join("splice"))
	rtest.Assert(t, !splice2.Subtree.Equal(*splice5.Subtree),
		"full walk did not pick up the change inside the subtree")
}
