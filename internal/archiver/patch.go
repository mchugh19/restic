package archiver

import (
	"os"
	"strings"
)

// PatchPlan tells the archiver to build the snapshot by patching the
// parent snapshot's tree instead of walking everything. It is the
// consumer half of project 6 (send-driven patch mode): an external
// producer supplies the paths that changed and the paths that were
// deleted since the parent snapshot, and the walk only lists the
// directories needed to reach those paths. Every other directory
// reuses its parent subtree by blob reference, without being opened.
//
// The plan is keyed by absolute target path (the recorded path, not a
// filesystem-internal one), exactly like SparsePlan. ListDirs holds
// every directory the walk must list: the target roots, every
// directory on the changed list, and every ancestor of a changed or
// deleted path down to its target root. The set is therefore closed
// under ancestors of every listed path, which is what makes splicing
// sound: a spliced directory has no listed descendant, so nothing
// below it can differ from the parent tree.
//
// Changed and Deleted record the lists the plan was built from. The
// backup command validates them against the filesystem and the
// parent tree before the walk (a changed path must exist, a deleted
// path must exist in the parent tree and no longer exist in the
// filesystem, every ancestor of a changed path must be a real
// directory); any entry that cannot be reconciled makes the command
// discard the plan and run a full walk instead. The archiver itself
// only needs ListDirs and Roots to walk; the two sets stay on the
// plan so the mode's inputs are visible wherever the plan is used.
type PatchPlan struct {
	Roots    []string
	ListDirs map[string]bool
	Changed  map[string]bool
	Deleted  map[string]bool
}

// Covers reports whether path is at or below one of the plan's roots.
func (p *PatchPlan) Covers(path string) bool {
	for _, r := range p.Roots {
		if path == r || strings.HasPrefix(path, r+string(os.PathSeparator)) {
			return true
		}
	}
	return false
}
