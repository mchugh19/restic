//go:build linux

// btrfsprobe is a temporary validation tool for the prototype's
// internal/btrfs package: it prints the D1/D2 changed-inode sets for a
// btrfs snapshot and the path of every changed inode, so the Go ioctl
// layer can be checked against the Python ground truth from the
// testbed experiments. Not part of the restic deliverable.
//
// Usage: btrfsprobe <snapshot-root> <subvolume-id> <g-prev>
package main

import (
	"fmt"
	"os"
	"sort"
	"strconv"

	"github.com/restic/restic/internal/btrfs"
)

func main() {
	if len(os.Args) != 4 {
		fmt.Fprintln(os.Stderr, "usage: btrfsprobe <snapshot-root> <subvolume-id> <g-prev>")
		os.Exit(2)
	}
	root := os.Args[1]
	treeID, err := strconv.ParseUint(os.Args[2], 10, 64)
	if err != nil {
		panic(err)
	}
	gPrev, err := strconv.ParseUint(os.Args[3], 10, 64)
	if err != nil {
		panic(err)
	}
	f, err := btrfs.OpenRoot(root)
	if err != nil {
		panic(err)
	}
	defer func() { _ = f.Close() }()

	d1, d2, err := btrfs.ChangedSets(f.Fd(), treeID, gPrev)
	if err != nil {
		panic(err)
	}
	fmt.Printf("g_prev=%d d1=%d d2=%d\n", gPrev, len(d1), len(d2))

	union := make(map[uint64]bool)
	for ino := range d1 {
		union[ino] = true
	}
	for ino := range d2 {
		union[ino] = true
	}
	inos := make([]uint64, 0, len(union))
	for ino := range union {
		inos = append(inos, ino)
	}
	sort.Slice(inos, func(i, j int) bool { return inos[i] < inos[j] })
	for _, ino := range inos {
		paths, err := btrfs.InoPaths(f.Fd(), ino)
		if err != nil {
			fmt.Printf("ino=%d d1=%v d2=%v paths=ERROR %v\n", ino, d1[ino], d2[ino], err)
			continue
		}
		fmt.Printf("ino=%d d1=%v d2=%v paths=%q\n", ino, d1[ino], d2[ino], paths)
	}
}
