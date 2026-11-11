package main

import (
	"encoding/base64"
	"encoding/hex"
	"math"
	"reflect"
	"testing"
)

func historyIDFixture() deltaID {
	id := deltaID{Timestamp: 256, Actor: historyActor{Tag: 1}}
	actor := uuidFixture(0)
	copy(id.Actor.ID[:], actor[:])
	return id
}

const historyIDWire = "92cd01009201c410000102030405060708090a0b0c0d0e0f"
const historyBatchWire = "94" + historyIDWire + "c09091920c911c"

func TestThreadIDAndHistoryRequestFixtures(t *testing.T) {
	raw := turnFixture(t, "92c410000102030405060708090a0b0c0d0e0f"+historyIDWire)
	id, err := parseThreadID(base64.RawURLEncoding.EncodeToString(raw))
	if err != nil || id.History != uuidFixture(0) || id.Timeline != historyIDFixture() {
		t.Fatalf("incorrect thread identity: %+v %v", id, err)
	}
	request, err := historySinceRequest(id, nil)
	if err != nil {
		t.Fatal(err)
	}
	wire, err := request.encode(42)
	if err != nil || hex.EncodeToString(wire) != "922a920693c410000102030405060708090a0b0c0d0e0f"+historyIDWire+"90" {
		t.Fatalf("incorrect history-index request: %x %v", wire, err)
	}
	request, err = historySinceRequest(id, []deltaID{historyIDFixture()})
	if err != nil {
		t.Fatal(err)
	}
	wire, err = request.encode(42)
	if err != nil || hex.EncodeToString(wire) != "922a920693c410000102030405060708090a0b0c0d0e0f"+historyIDWire+"91"+historyIDWire {
		t.Fatalf("incorrect incremental index request: %x %v", wire, err)
	}
	request, err = historyDeltasRequest(id.History, turnFixture(t, "91c402dead"))
	if err != nil {
		t.Fatal(err)
	}
	wire, err = request.encode(42)
	if err != nil || hex.EncodeToString(wire) != "922a921b92c410000102030405060708090a0b0c0d0e0f91c402dead" {
		t.Fatalf("incorrect delta request: %x %v", wire, err)
	}
}

func TestRejectInvalidHistoryIdentities(t *testing.T) {
	for _, value := range []string{
		"", "not-a-thread-id", "https://delta.dev/join/not-a-thread",
		base64.RawURLEncoding.EncodeToString([]byte{0xc0}),
		base64.RawURLEncoding.EncodeToString(turnFixture(t, "92c410000102030405060708090a0b0c0d0e0f92ff9201c410000102030405060708090a0b0c0d0e0f")),
	} {
		if _, err := parseThreadID(value); err == nil {
			t.Errorf("accepted invalid thread ID %q", value)
		}
	}
	id := historyIDFixture()
	id.Actor.Tag = 2
	if _, err := id.wireValue(); err == nil {
		t.Fatal("encoded unknown actor")
	}
	id = historyIDFixture()
	id.Actor.ID[19] = 1
	if _, err := id.wireValue(); err == nil {
		t.Fatal("encoded noncanonical UUID actor")
	}
	for _, value := range []string{"90", "c0", "91a26162", "91c0", "ddffffffff"} {
		if _, err := historyDeltasRequest(uuidFixture(0), turnFixture(t, value)); err == nil {
			t.Errorf("accepted invalid compressed IDs %s", value)
		}
	}
}

func TestHistoryDeltaBatchDecoding(t *testing.T) {
	for _, wire := range []string{
		historyBatchWire,
		"95" + historyBatchWire[2:] + "a6667574757265",
	} {
		batch, err := decodeHistoryBatch(turnFixture(t, wire))
		if err != nil || batch.ID != historyIDFixture() || len(batch.Parent) != 0 ||
			len(batch.Payloads) != 1 || batch.Payloads[0].Tag != 12 ||
			hex.EncodeToString(batch.Payloads[0].Payload) != "911c" {
			t.Fatalf("incorrect batch decoding: %+v %v", batch, err)
		}
	}
	for _, wire := range []string{"90", "94" + historyIDWire + "c0c090", historyBatchWire + "00"} {
		if _, err := decodeHistoryBatch(turnFixture(t, wire)); err == nil {
			t.Errorf("accepted malformed history batch %s", wire)
		}
	}
}

func TestHistoryDownloadVariants(t *testing.T) {
	for _, wire := range []string{
		"92009191" + historyBatchWire,          // Deltas {deltas}.
		"9202930191" + historyBatchWire + "90", // DeltasAndReplicationPacks.
	} {
		download, err := decodeHistoryDownload(turnFixture(t, wire))
		if err != nil || download.CoveredBatchCount != 1 || len(download.Batches) != 1 ||
			len(download.ReplicationPacks) != 0 || download.Batches[0].ID != historyIDFixture() {
			t.Fatalf("incorrect history download: %+v %v", download, err)
		}
	}
	for _, wire := range []string{"01", "92039100", "9204911c", "63", "920293cf00000001000000009090"} {
		if _, err := decodeHistoryDownload(turnFixture(t, wire)); err == nil {
			t.Errorf("non-success history response accepted: %s", wire)
		}
	}
	// A pack reference must remain visible, not be mistaken for inline batches
	// or silently discarded as proof that history has been fully downloaded.
	download, err := decodeHistoryDownload(turnFixture(t, "92029302909191a47061636b"))
	if err != nil || download.CoveredBatchCount != 2 || len(download.Batches) != 0 ||
		len(download.ReplicationPacks) != 1 {
		t.Fatalf("lost replication-pack reference: %+v %v", download, err)
	}
}

func TestHistorySequencesAreNotStructs(t *testing.T) {
	values := make([]any, 1025)
	for i := range values {
		values[i] = uint64(math.MaxUint64)
	}
	wire, err := encodeWire(values)
	if err != nil {
		t.Fatal(err)
	}
	sequence, err := decodeSequence(wire, maxHistoryItems)
	if err != nil || len(sequence) != len(values) || !reflect.DeepEqual(sequence[0], sequence[1024]) {
		t.Fatalf("history sequence was truncated or rejected: %d %v", len(sequence), err)
	}
	if _, err := decodeSequence(wire, 1024); err == nil {
		t.Fatal("sequence bound ignored")
	}
}

func TestHistoryInspectionRequiresSealedThreadAuthorization(t *testing.T) {
	raw := turnFixture(t, "92c410000102030405060708090a0b0c0d0e0f"+historyIDWire)
	id := base64.RawURLEncoding.EncodeToString(raw)
	for _, identity := range []runnerIdentity{
		{},
		{AllowedThreads: []string{"a different thread"}},
	} {
		if _, err := identity.authorizeHistory(id); err == nil {
			t.Fatal("caller selected a thread outside the credential grant")
		}
	}
	identity := runnerIdentity{AllowedThreads: []string{id}}
	target, err := identity.authorizeHistory(id)
	if err != nil || target.History != uuidFixture(0) || target.Timeline != historyIDFixture() {
		t.Fatalf("authorized history was not decoded: %+v %v", target, err)
	}
	identity.AllowedThreads = []string{"invalid"}
	if _, err := identity.authorizeHistory("invalid"); err == nil {
		t.Fatal("invalid sealed identity became an authorized request")
	}
}
