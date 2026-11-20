package main

import (
	"bytes"
	"encoding/hex"
	"math"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func TestHistoryWriteMatchesNativeReference(t *testing.T) {
	thread := threadID{History: uuidFixture(0), Timeline: historyIDFixture()}
	request, err := historyUpdateRequest(thread, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	wire, err := request.encode(42)
	if err != nil || hex.EncodeToString(wire) != "922a920195c410000102030405060708090a0b0c0d0e0f90909090" {
		t.Fatalf("wrong no-op history envelope: %x %v", wire, err)
	}
	next := historyIDFixture()
	next.Timestamp++
	batch := historyDeltaBatch{
		ID: next, Parent: []deltaID{historyIDFixture()}, Payloads: []wireVariant{replayPayload(t, 5)},
	}
	request, err = historyUpdateRequest(thread, []historyDeltaBatch{batch}, []deltaID{next})
	if err != nil {
		t.Fatal(err)
	}
	wire, err = request.encode(42)
	id := "92cd01019201c410000102030405060708090a0b0c0d0e0f"
	batchWire := "94" + id + "c091" + historyIDWire + "91920590"
	expected := "922a920195c410000102030405060708090a0b0c0d0e0f91" + batchWire +
		"9192" + historyIDWire + "91" + id + "9090"
	if err != nil || hex.EncodeToString(wire) != expected {
		t.Fatalf("wrong causal history write: %x %v", wire, err)
	}
}

func TestHistoryBatchAllocatesClockAndRetainsFrontier(t *testing.T) {
	head, sibling, timeline := replayID(7, 1), replayID(30, 2), replayID(0, 1)
	timelineWire, _ := timeline.wireValue()
	headWire, _ := head.wireValue()
	metadata, _ := encodeWire([]any{timelineWire, []any{headWire}})
	snapshot := historySnapshot{
		Timeline: timeline, Timelines: []msgpack.RawMessage{metadata},
		Download: historyDownload{CoveredBatchCount: 2, Batches: []historyDeltaBatch{
			{ID: head}, {ID: sibling},
		}},
	}
	actor := uuidFixture(10)
	payloads := []wireVariant{replayPayload(t, 5), replayPayload(t, 12, uint64(28))}
	batch, err := newHistoryBatch(snapshot, actor, payloads)
	if err != nil {
		t.Fatal(err)
	}
	if batch.ID.Timestamp != 32 || batch.ID.Actor.Tag != 1 ||
		!bytes.Equal(batch.ID.Actor.ID[:16], actor[:]) ||
		len(batch.Parent) != 1 || batch.Parent[0] != head {
		t.Fatal("batch lost its frontier, actor or history-wide clock")
	}
	first, err := payloadID(batch, 0)
	if err != nil || first.Timestamp != 31 {
		t.Fatalf("payload clock span overlaps an existing branch: %v", err)
	}
	payloads[0].Payload[0] = 0xc0
	if !bytes.Equal(batch.Payloads[0].Payload, []byte{0x90}) {
		t.Fatal("caller mutation changed an allocated write")
	}
	snapshot.Download.Batches[1].ID.Timestamp = math.MaxUint64
	if _, err := newHistoryBatch(snapshot, actor, payloads); err == nil {
		t.Fatal("accepted an overflowing history clock")
	}
	snapshot.Download.Batches = snapshot.Download.Batches[:1]
	if _, err := newHistoryBatch(snapshot, actor, payloads); err == nil {
		t.Fatal("accepted a truncated history")
	}
	snapshot.Download.CoveredBatchCount = 1
	snapshot.Download.ReplicationPacks = []msgpack.RawMessage{{0x90}}
	if _, err := newHistoryBatch(snapshot, actor, payloads); err == nil {
		t.Fatal("allocated against an unfetched replication pack")
	}
	snapshot.Download.ReplicationPacks = nil
	snapshot.Timelines = nil
	if _, err := newHistoryBatch(snapshot, actor, payloads); err == nil {
		t.Fatal("allocated without a timeline frontier")
	}
	if _, err := newHistoryBatch(snapshot, actor, nil); err == nil {
		t.Fatal("allocated an empty write")
	}
}
