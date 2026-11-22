package main

import (
	"bytes"
	"math"
	"math/rand"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func replayID(timestamp uint64, actor byte) deltaID {
	id := deltaID{Timestamp: timestamp, Actor: historyActor{Tag: 1}}
	id.Actor.ID[0] = actor
	return id
}

func replayPayload(t *testing.T, tag uint64, fields ...any) wireVariant {
	t.Helper()
	if fields == nil {
		fields = []any{}
	}
	data, err := encodeWire(fields)
	if err != nil {
		t.Fatal(err)
	}
	return wireVariant{Tag: tag, Payload: data}
}

func replayEdit(t *testing.T, text string, coordinates ...uint64) wireVariant {
	t.Helper()
	fields := []any{text}
	for _, value := range coordinates {
		fields = append(fields, value)
	}
	data, err := encodeWire(fields)
	if err != nil {
		t.Fatal(err)
	}
	return replayPayload(t, 2, []any{1, []byte{0}}, msgpack.RawMessage(data))
}

func replayTextAt(t *testing.T, batches []historyDeltaBatch, heads ...deltaID) string {
	t.Helper()
	state, err := replayHistory(batches, heads)
	if err != nil {
		t.Fatal(err)
	}
	buffer := state.Buffers[fieldObject(0)]
	if buffer == nil {
		return ""
	}
	return buffer.stringAt(state.basis)
}

func TestReplayConcurrentTextMatchesReference(t *testing.T) {
	base, a, b := replayID(1, 1), replayID(2, 1), replayID(2, 2)
	cases := []struct {
		name     string
		initial  wireVariant
		left     wireVariant
		right    wireVariant
		expected string
	}{
		{"concurrent insertion", replayEdit(t, "", 0, 0, 0),
			replayEdit(t, "X", 0, 0, 1), replayEdit(t, "Y", 0, 0, 1), "YX"},
		{"delete with unseen insertion", replayEdit(t, "abcd", 0, 0, 4),
			replayEdit(t, "", 1, 2, 0), replayEdit(t, "X", 2, 0, 1), "aXd"},
		{"UTF-8 byte offsets", replayEdit(t, "😀é", 0, 0, 6),
			replayEdit(t, "X", 4, 0, 1), replayEdit(t, "", 0, 4, 0), "Xé"},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			batches := []historyDeltaBatch{
				{ID: base, Payloads: []wireVariant{test.initial}},
				{ID: a, Parent: []deltaID{base}, Payloads: []wireVariant{test.left}},
				{ID: b, Parent: []deltaID{base}, Payloads: []wireVariant{test.right}},
			}
			random := rand.New(rand.NewSource(7))
			for i := 0; i < 20; i++ {
				random.Shuffle(len(batches), func(i, j int) { batches[i], batches[j] = batches[j], batches[i] })
				if actual := replayTextAt(t, batches, a, b); actual != test.expected {
					t.Fatalf("wrong concurrent text: %q, want %q", actual, test.expected)
				}
			}
		})
	}
}

func TestReplayCausalityIsNotAnActorClockPrefix(t *testing.T) {
	base, left, right, next := replayID(1, 1), replayID(3, 1), replayID(4, 1), replayID(5, 1)
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayEdit(t, "a", 0, 0, 1)}},
		{ID: left, Parent: []deltaID{base}, Payloads: []wireVariant{replayEdit(t, "X", 1, 0, 1)}},
		{ID: right, Parent: []deltaID{base}, Payloads: []wireVariant{replayEdit(t, "Y", 1, 0, 1)}},
		{ID: next, Parent: []deltaID{left}, Payloads: []wireVariant{replayEdit(t, "Z", 2, 0, 1)}},
	}
	if actual := replayTextAt(t, batches, left); actual != "aX" {
		t.Fatalf("unrelated fork leaked into selected timeline: %q", actual)
	}
	if actual := replayTextAt(t, batches, next, right); actual != "aYXZ" {
		t.Fatalf("edit used a noncausal parent view: %q", actual)
	}
}

func TestReplayMultiplePayloadsAndAbsoluteSplicePositions(t *testing.T) {
	id := replayID(42, 1)
	batch := historyDeltaBatch{ID: id, Payloads: []wireVariant{
		replayPayload(t, 12, uint64(28)),
		replayEdit(t, "abcdef", 0, 0, 6),
		replayEdit(t, "XY", 2, 0, 1, 3, 0, 1),
	}}
	for index, expected := range []uint64{40, 41, 42} {
		value, err := payloadID(batch, index)
		if err != nil || value.Timestamp != expected {
			t.Fatalf("wrong payload identity: %+v %v", value, err)
		}
	}
	if actual := replayTextAt(t, []historyDeltaBatch{batch}, id); actual != "abXcYdef" {
		t.Fatalf("wrong absolute-position edit: %q", actual)
	}
	if _, err := payloadID(batch, 3); err == nil {
		t.Fatal("out-of-range payload identity accepted")
	}
	batch.ID.Timestamp = 1
	if _, err := payloadID(batch, 0); err == nil {
		t.Fatal("payload clock underflow accepted")
	}
}

func TestPersistentHistoryAncestry(t *testing.T) {
	var original *historyAncestry
	for _, index := range []int{0, 63, 64, 4095, 4096, maxHistoryItems - 1} {
		original = addAncestor(original, index, ancestryDepth)
	}
	a := addAncestor(original, 3, ancestryDepth)
	b := addAncestor(original, 7000, ancestryDepth)
	union := unionAncestors(a, b)
	if hasAncestor(original, 3) || hasAncestor(original, 7000) ||
		!hasAncestor(union, 3) || !hasAncestor(union, 7000) {
		t.Fatal("persistent ancestry leaked a write or lost a merge")
	}
	for _, index := range []int{0, 63, 64, 4095, 4096, maxHistoryItems - 1} {
		if !hasAncestor(original, index) || !hasAncestor(union, index) {
			t.Fatalf("lost ancestry bit %d", index)
		}
	}
	if unionAncestors(original, original) != original {
		t.Fatal("identical ancestry did not share its tree")
	}
}

func TestTextEditRejectsMalformedLengthsAndUTF8(t *testing.T) {
	for _, value := range []any{
		[]any{}, []any{"abc", 0}, []any{"abc", 0, 0, 2},
		[]any{"abc", uint64(math.MaxUint64), 0, 3},
		[]any{string([]byte{0xff}), 0, 0, 1},
	} {
		raw, err := encodeWire(value)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := decodeTextEdit(raw); err == nil {
			t.Fatal("malformed text edit accepted")
		}
	}
	bad := historyDeltaBatch{ID: replayID(1, 1), Payloads: []wireVariant{replayEdit(t, "é", 0, 0, 1)}}
	if _, err := replayHistory([]historyDeltaBatch{bad}, []deltaID{bad.ID}); err == nil {
		t.Fatal("split UTF-8 insertion accepted")
	}
	if _, err := decodeString([]byte{0xa1, 0xff}); err == nil {
		t.Fatal("invalid protocol UTF-8 accepted")
	}
	valid, _ := decodeTextEdit([]byte{0x91, 0xa0})
	if valid.Text != "" || len(valid.Splices) != 0 || !bytes.Equal([]byte(valid.Text), []byte{}) {
		t.Fatal("empty edit did not decode")
	}
}
