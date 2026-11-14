package main

import "testing"

func TestAnchorResolutionMatchesNativeByteBias(t *testing.T) {
	base, insert := replayID(1, 1), replayID(2, 1)
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayEdit(t, "abc", 0, 0, 3)}},
		{ID: insert, Parent: []deltaID{base}, Payloads: []wireVariant{replayEdit(t, "X", 1, 0, 1)}},
	}
	state, err := replayHistory(batches, []deltaID{insert})
	if err != nil {
		t.Fatal(err)
	}
	for index, before := range []int{0, 2, 3} {
		for _, right := range []bool{false, true} {
			expected := before
			if !right {
				expected++
			}
			anchor := scratchAnchor{Kind: 2, Buffer: fieldObject(0), Insertion: base, Offset: uint64(index), RightBias: right}
			actual, err := state.resolveAnchor(anchor)
			if err != nil || actual != expected {
				t.Fatalf("wrong biased byte offset: %d %v, want %d", actual, err, expected)
			}
		}
	}
}

func TestDeletedAndUnicodeAnchors(t *testing.T) {
	base, remove := replayID(1, 1), replayID(2, 1)
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayEdit(t, "😀é", 0, 0, 6)}},
		{ID: remove, Parent: []deltaID{base}, Payloads: []wireVariant{replayEdit(t, "", 0, 4, 0)}},
	}
	state, err := replayHistory(batches, []deltaID{remove})
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		byteOffset uint64
		right      bool
		expected   int
	}{
		{0, false, 0}, {3, true, 0}, {4, true, 0}, {5, false, 2},
	} {
		anchor := scratchAnchor{Kind: 2, Buffer: fieldObject(0), Insertion: base, Offset: test.byteOffset, RightBias: test.right}
		actual, err := state.resolveAnchor(anchor)
		if err != nil || actual != test.expected {
			t.Fatalf("wrong deleted/Unicode anchor: %d %v", actual, err)
		}
	}
	if _, err := state.resolveAnchor(scratchAnchor{Kind: 2, Buffer: fieldObject(0), Insertion: base, Offset: 6}); err == nil {
		t.Fatal("past-end insertion offset was accepted")
	}
	if offset, err := state.resolveAnchor(scratchAnchor{Kind: 1, Buffer: fieldObject(0)}); err != nil || offset != 2 {
		t.Fatal("end anchor did not use current visible length")
	}
}
