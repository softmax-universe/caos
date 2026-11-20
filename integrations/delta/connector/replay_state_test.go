package main

import (
	"bytes"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func TestReplayRegistersMapsAndTombstones(t *testing.T) {
	base, left, right := replayID(1, 1), replayID(2, 1), replayID(2, 2)
	insert, remove, replace := replayID(3, 1), replayID(4, 1), replayID(4, 2)
	baseWire, _ := base.wireValue()
	mapID := []any{0, baseWire}
	register := []any{1, []byte{32}}
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayPayload(t, 5)}},
		{ID: left, Parent: []deltaID{base}, Payloads: []wireVariant{replayPayload(t, 4, register, []byte{7})}},
		{ID: right, Parent: []deltaID{base}, Payloads: []wireVariant{replayPayload(t, 4, register, []byte{9})}},
		{ID: insert, Parent: []deltaID{left, right}, Payloads: []wireVariant{replayPayload(t, 6, mapID, []byte{1}, []byte{7})}},
		{ID: remove, Parent: []deltaID{insert}, Payloads: []wireVariant{replayPayload(t, 7, mapID, []byte{1})}},
		{ID: replace, Parent: []deltaID{insert}, Payloads: []wireVariant{replayPayload(t, 6, mapID, []byte{1}, []byte{9})}},
	}
	state, err := replayHistory(batches, []deltaID{remove, replace})
	if err != nil {
		t.Fatal(err)
	}
	value := state.Registers[fieldObject(32)]
	if value.Write != right || !value.Present || !bytes.Equal(value.Bytes, []byte{9}) {
		t.Fatalf("wrong register winner: %+v", value)
	}
	entry := state.Maps[objectID{Delta: base}].get(string([]byte{1}), state.basis)
	if entry.Write != replace || !entry.Present || !bytes.Equal(entry.Bytes, []byte{9}) {
		t.Fatalf("wrong map winner: %+v", entry)
	}
	state, err = replayHistory(batches, []deltaID{remove})
	if err != nil {
		t.Fatal(err)
	}
	if state.Maps[objectID{Delta: base}].get(string([]byte{1}), state.basis).Present {
		t.Fatal("unseen concurrent replacement defeated a selected tombstone")
	}
}

func TestReplayPreservesSealsAndCreationValues(t *testing.T) {
	create, seal := replayID(1, 1), replayID(2, 1)
	ref := []any{1, []byte{0}}
	buffer := []any{0, ref}
	start, end := []any{0, []any{buffer}}, []any{1, []any{buffer}}
	batches := []historyDeltaBatch{
		{ID: create, Payloads: []wireVariant{replayEdit(t, "abc", 0, 0, 3)}},
		{ID: seal, Parent: []deltaID{create}, Payloads: []wireVariant{
			replayPayload(t, 18, ref, []any{[]any{start, end}}),
		}},
	}
	state, err := replayHistory(batches, []deltaID{seal})
	if err != nil {
		t.Fatal(err)
	}
	if len(state.Seals) != 1 || len(state.Seals[0].Ranges) != 1 ||
		state.Seals[0].Ranges[0].Start.Buffer != fieldObject(0) ||
		!state.Seals[0].Basis.contains(historyPosition{Batch: 0, Payload: 0}) {
		t.Fatal("seal or its causal protection context was lost")
	}
	if text := state.Buffers[fieldObject(0)].stringAt(state.basis); text != "abc" {
		t.Fatalf("a seal changed text: %q", text)
	}
	register, scratch := replayID(3, 1), replayID(4, 1)
	batches = append(batches,
		historyDeltaBatch{ID: register, Parent: []deltaID{seal}, Payloads: []wireVariant{replayPayload(t, 3, []byte{42})}},
		historyDeltaBatch{ID: scratch, Parent: []deltaID{register}, Payloads: []wireVariant{replayPayload(t, 1, "created")}},
	)
	state, err = replayHistory(batches, []deltaID{scratch})
	if err != nil || !bytes.Equal(state.Registers[objectID{Delta: register}].Bytes, []byte{42}) ||
		state.Buffers[objectID{Delta: scratch}].stringAt(state.basis) != "created" {
		t.Fatalf("creation delta was not replayed: %v", err)
	}
}

func TestMapRemovalPreservesUnseenConcurrentValues(t *testing.T) {
	base, left, right, remove := replayID(1, 1), replayID(2, 1), replayID(3, 2), replayID(4, 2)
	ref := []any{1, []byte{1}}
	key := []byte("k")
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayPayload(t, 12, uint64(28))}},
		{ID: left, Parent: []deltaID{base}, Payloads: []wireVariant{replayPayload(t, 6, ref, key, []byte("A"))}},
		{ID: right, Parent: []deltaID{base}, Payloads: []wireVariant{replayPayload(t, 6, ref, key, []byte("B"))}},
		{ID: remove, Parent: []deltaID{right}, Payloads: []wireVariant{replayPayload(t, 7, ref, key)}},
	}
	state, err := replayHistory(batches, []deltaID{left, remove})
	if err != nil {
		t.Fatal(err)
	}
	entry := state.Maps[fieldObject(1)].get(string(key), state.basis)
	if !entry.Present || entry.Write != left || string(entry.Bytes) != "A" {
		t.Fatalf("removing an observed winner erased an unseen concurrent value: %+v", entry)
	}
	batches[3].Parent = []deltaID{base}
	state, err = replayHistory(batches, []deltaID{left, remove})
	if err != nil || !state.Maps[fieldObject(1)].get(string(key), state.basis).Present {
		t.Fatalf("empty-parent removal erased an unseen insertion: %v", err)
	}
}

func TestReplayRejectsIncompleteOrContradictoryHistory(t *testing.T) {
	a, b := replayID(1, 1), replayID(2, 1)
	one := historyDeltaBatch{ID: a, Payloads: []wireVariant{replayEdit(t, "a", 0, 0, 1)}}
	for _, batches := range [][]historyDeltaBatch{
		{{ID: b, Parent: []deltaID{a}}},
		{{ID: a, Parent: []deltaID{b}}, {ID: b, Parent: []deltaID{a}}},
		{one, {ID: a, Payloads: []wireVariant{replayEdit(t, "b", 0, 0, 1)}}},
		{one, {ID: b, Parent: []deltaID{a}, Payloads: []wireVariant{replayEdit(t, "b", 0, 0, 1), replayEdit(t, "c", 0, 0, 1)}}},
		{{ID: b, Payloads: []wireVariant{{Tag: 255}}}},
	} {
		if _, err := replayHistory(batches, []deltaID{b}); err == nil {
			t.Fatal("incomplete or contradictory history accepted")
		}
	}
	if actual := replayTextAt(t, []historyDeltaBatch{one, one}, a); actual != "a" {
		t.Fatal("duplicate delivery applied a batch twice")
	}
}

func TestSnapshotReplayUsesTimelineFrontier(t *testing.T) {
	id := replayID(1, 1)
	timeline := replayID(0, 1)
	timelineWire, _ := timeline.wireValue()
	head, _ := id.wireValue()
	metadata, err := encodeWire([]any{timelineWire, []any{head}})
	if err != nil {
		t.Fatal(err)
	}
	snapshot := historySnapshot{
		Timeline:  timeline,
		Timelines: []msgpack.RawMessage{metadata},
		Download: historyDownload{CoveredBatchCount: 1, Batches: []historyDeltaBatch{
			{ID: id, Payloads: []wireVariant{replayEdit(t, "a", 0, 0, 1)}},
		}},
	}
	state, err := replaySnapshot(snapshot)
	if err != nil || state.Buffers[fieldObject(0)].stringAt(state.basis) != "a" {
		t.Fatalf("snapshot frontier was not replayed: %v", err)
	}
	snapshot.Download.ReplicationPacks = []msgpack.RawMessage{{0x90}}
	if _, err := replaySnapshot(snapshot); err == nil {
		t.Fatal("pack references were mistaken for complete history")
	}
	snapshot.Download.ReplicationPacks = nil
	snapshot.Timelines = nil
	if _, err := replaySnapshot(snapshot); err == nil {
		t.Fatal("missing timeline metadata accepted")
	}
}
