package main

import (
	"bytes"
	"encoding/hex"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func workspaceEntry(t *testing.T, tag uint64, fields ...any) msgpack.RawMessage {
	t.Helper()
	data, err := encodeWire([]any{tag, fields})
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func TestWorkspaceTreeUsesGitObjectIdentity(t *testing.T) {
	blob := hashGitObject("blob", []byte("hello\n"))
	if hex.EncodeToString(blob[:]) != "ce013625030ba8dba906f756967f9e9ca394464a" {
		t.Fatal("wrong Git blob identity")
	}
	entries := []msgpack.RawMessage{
		workspaceEntry(t, 0, ""),
		workspaceEntry(t, 1, "a.c", []any{0, blob[:]}, uint64(0)),
		workspaceEntry(t, 1, "a/z", []any{0, blob[:]}, uint64(1)),
		workspaceEntry(t, 2, "link", "../a.c"),
	}
	tree, err := buildWorkspaceTree(entries)
	if err != nil {
		t.Fatal(err)
	}
	// Git sorts a directory as "a/", after "a.c", not as the bare name "a".
	subtree := append([]byte("100755 z\x00"), blob[:]...)
	subOID := hashGitObject("tree", subtree)
	linkOID := hashGitObject("blob", []byte("../a.c"))
	root := append([]byte("100644 a.c\x00"), blob[:]...)
	root = append(root, []byte("40000 a\x00")...)
	root = append(root, subOID[:]...)
	root = append(root, []byte("120000 link\x00")...)
	root = append(root, linkOID[:]...)
	if tree.Root != hashGitObject("tree", root) ||
		!bytes.Equal(tree.Objects[tree.Root].Data, root) ||
		!bytes.Equal(tree.Objects[subOID].Data, subtree) ||
		string(tree.Objects[linkOID].Data) != "../a.c" {
		t.Fatal("Git tree identity, ordering or mode was lost")
	}
	if _, copied := tree.Objects[blob]; copied {
		t.Fatal("an existing file blob was replaced with invented contents")
	}
	entries[0], entries[3] = entries[3], entries[0]
	reordered, err := buildWorkspaceTree(entries)
	if err != nil || reordered.Root != tree.Root {
		t.Fatalf("entry delivery order changed Git tree: %v", err)
	}
}

func TestWorkspaceTreeRejectsUnsafeOrConflictingEntries(t *testing.T) {
	oid := make([]byte, 20)
	file := func(path string) msgpack.RawMessage {
		return workspaceEntry(t, 1, path, []any{0, oid}, uint64(0))
	}
	for _, path := range []string{"", "/etc/passwd", "a/../b", "./a", "a//b", "a/", ".git/config", "a\x00b"} {
		if _, err := buildWorkspaceTree([]msgpack.RawMessage{file(path)}); err == nil {
			t.Fatalf("accepted unsafe file path %q", path)
		}
	}
	for _, entries := range [][]msgpack.RawMessage{
		{file("a"), file("a")},
		{file("a"), file("a/b")},
		{file("a/b"), file("a")},
		{workspaceEntry(t, 2, "a", "b"), file("a/b")},
		{workspaceEntry(t, 2, "a", "bad\x00target")},
		{workspaceEntry(t, 1, "a", []any{1, []any{}}, uint64(0))},
		{workspaceEntry(t, 1, "a", []any{0, oid}, uint64(2))},
		{workspaceEntry(t, 1, "a", []any{0, []byte{1}}, uint64(0))},
		{workspaceEntry(t, 3, "a")},
	} {
		if _, err := buildWorkspaceTree(entries); err == nil {
			t.Fatal("accepted conflicting or unsupported repository entry")
		}
	}
}

func TestWorkspaceReconstructionUsesPinnedVersion(t *testing.T) {
	create, later := replayID(1, 1), replayID(2, 1)
	repository := []byte("repository identity")
	head := hashGitObject("commit", []byte("test commit"))
	reference := workspaceReference{Repository: repository, Worktree: create, Version: []deltaID{create}}
	download := historyDownload{CoveredBatchCount: 2, Batches: []historyDeltaBatch{
		{ID: create, Payloads: []wireVariant{replayPayload(t, 8, repository,
			[]any{[]any{head[:], []any{}, []any{}, []any{}}}, []any{})}},
		// An unsupported future edit must not contaminate an older pinned tree.
		{ID: later, Parent: []deltaID{create}, Payloads: []wireVariant{{Tag: 9}}},
	}}
	workspace, err := reconstructInitialWorkspace(reference, download)
	if err != nil || workspace.Head == nil || *workspace.Head != head ||
		workspace.Tree.Root != hashGitObject("tree", nil) {
		t.Fatalf("could not reconstruct the pinned workspace: %v", err)
	}
	reference.Version = []deltaID{later}
	if _, err := reconstructInitialWorkspace(reference, download); err == nil {
		t.Fatal("silently dropped an incremental edit")
	}
	reference.Version = []deltaID{create}
	reference.Repository = []byte("another repository")
	if _, err := reconstructInitialWorkspace(reference, download); err == nil {
		t.Fatal("accepted a mismatched repository identity")
	}
	reference.Repository = repository
	reference.Worktree = later
	if _, err := reconstructInitialWorkspace(reference, download); err == nil {
		t.Fatal("used a different worktree's creation")
	}
	reference.Worktree = create
	download.ReplicationPacks = []msgpack.RawMessage{{0x90}}
	if _, err := reconstructInitialWorkspace(reference, download); err == nil {
		t.Fatal("treated pack references as complete workspace history")
	}
}

func TestThreadWorkspaceDescriptorsPreserveIdentityAndOrder(t *testing.T) {
	history, labelA, labelB := uuidFixture(20), uuidFixture(40), uuidFixture(60)
	timeline, worktree := replayID(1, 1), replayID(2, 1)
	timelineWire, _ := timeline.wireValue()
	worktreeWire, _ := worktree.wireValue()
	root, _ := encodeWire(worktreeWire)
	stub := []any{history[:], timelineWire, []any{worktreeWire}, root}
	resolved := []any{1, []any{[]byte("repo"), stub, []any{}, []any{}}}
	keyA, _ := encodeWire(labelA[:])
	keyB, _ := encodeWire(labelB[:])
	valueA, _ := encodeWire([]any{uint64(9), resolved})
	valueB, _ := encodeWire([]any{uint64(2), resolved})
	at := replayID(10, 1)
	payloads := []wireVariant{
		replayPayload(t, 6, []any{1, []byte{43}}, keyA, valueA),
		replayPayload(t, 6, []any{1, []byte{43}}, keyB, valueB),
	}
	state, err := replayHistory([]historyDeltaBatch{{ID: at, Payloads: payloads}}, []deltaID{at})
	if err != nil {
		t.Fatal(err)
	}
	references, err := threadWorkspaces(state)
	if err != nil || len(references) != 2 {
		t.Fatalf("workspace attachments not decoded: %v", err)
	}
	if references[0].Label != labelB || references[1].Label != labelA ||
		references[0].History != history || references[0].Timeline != timeline ||
		references[0].Worktree != worktree || len(references[0].Version) != 1 ||
		references[0].Version[0] != worktree || string(references[0].Repository) != "repo" {
		t.Fatal("workspace attachment lost its label, pinned version or identity")
	}
	for _, status := range []any{uint64(2), []any{0, []any{[]byte("repo"), history[:]}}} {
		value, _ := encodeWire([]any{uint64(0), status})
		state, err = replayHistory([]historyDeltaBatch{{ID: at, Payloads: []wireVariant{
			replayPayload(t, 6, []any{1, []byte{43}}, keyA, value),
		}}}, []deltaID{at})
		if err != nil {
			t.Fatal(err)
		}
		references, err = threadWorkspaces(state)
		if _, pending := status.([]any); pending {
			if err == nil {
				t.Fatal("pending workspace was mistaken for a resolved descriptor")
			}
		} else if err != nil || len(references) != 0 {
			t.Fatal("removed workspace was still attached")
		}
	}
}
