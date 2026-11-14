package main

import (
	"reflect"
	"strings"
	"testing"
)

func conversationFixture(t *testing.T) (*replayState, []historyDeltaBatch, []byte) {
	t.Helper()
	base, messages := replayID(1, 1), replayID(10, 1)
	text := "USER\n\nASSISTANT\n\nNEXT"
	write, _ := base.wireValue()
	buffer := []any{0, []any{1, []byte{0}}}
	at := func(offset int) any {
		return []any{2, []any{buffer, []any{[]any{write, false}, uint64(offset)}, uint64(1)}}
	}
	payload := func(offset int, kind uint64, fields []any) wireVariant {
		key, _ := encodeWire(at(offset))
		value, _ := encodeWire([]any{kind, fields})
		return replayPayload(t, 6, []any{1, []byte{1}}, key, value)
	}
	first := []any{
		[]any{}, []any{}, []any{"SECRET VOUCHER, NOT MODEL CONTEXT"}, true,
		[]any{[]any{at(0), at(4)}},
	}
	assistant := make([]any, 9)
	for index := range assistant {
		assistant[index] = []any{}
	}
	assistant[8] = []any{uint64(123)}
	batches := []historyDeltaBatch{
		{ID: base, Payloads: []wireVariant{replayEdit(t, text, 0, 0, uint64(len(text)))}},
		{ID: messages, Parent: []deltaID{base}, Payloads: []wireVariant{
			payload(17, 0, []any{}), payload(6, 1, assistant), payload(0, 0, first),
		}},
	}
	state, err := replayHistory(batches, []deltaID{messages})
	if err != nil {
		t.Fatal(err)
	}
	key, _ := encodeWire(at(6))
	return state, batches, key
}

func TestConversationProjectionKeepsRolesRangesAndPrivateMetadataSeparate(t *testing.T) {
	state, _, _ := conversationFixture(t)
	messages, err := projectConversation(state)
	if err != nil {
		t.Fatal(err)
	}
	if len(messages) != 3 {
		t.Fatalf("got %d messages", len(messages))
	}
	for index, expected := range []agentMessage{
		{Role: "user", Text: "USER", Complete: true},
		{Role: "assistant", Text: "ASSISTANT", Complete: true},
		{Role: "user", Text: "NEXT", Complete: true},
	} {
		actual := messages[index]
		if actual.Role != expected.Role || actual.Text != expected.Text ||
			actual.Complete != expected.Complete || len(actual.ID) != 32 {
			t.Fatalf("wrong projected message %d: %+v", index, actual)
		}
		if strings.Contains(actual.Text, "VOUCHER") {
			t.Fatal("private user metadata reached the model context")
		}
	}
	again, err := projectConversation(state)
	if err != nil || !reflect.DeepEqual(messages, again) {
		t.Fatal("map iteration changed message order or identity")
	}
}

func TestConversationProjectionDoesNotInventStructuredContext(t *testing.T) {
	state, batches, assistantKey := conversationFixture(t)
	original := batches[len(batches)-1]
	for _, field := range []byte{9, 20, 60, 65, 79} {
		at := replayID(20, 1)
		next := historyDeltaBatch{ID: at, Parent: []deltaID{original.ID}, Payloads: []wireVariant{
			replayPayload(t, 6, []any{1, []byte{field}}, []byte{1}, []byte{0}),
		}}
		state, err := replayHistory(append(batches, next), []deltaID{at})
		if err != nil {
			t.Fatal(err)
		}
		if _, err := projectConversation(state); err == nil {
			t.Fatalf("silently omitted unsupported content in field %d", field)
		}
	}
	at := replayID(20, 1)
	tool, _ := encodeWire([]any{uint64(3), []any{}})
	next := historyDeltaBatch{ID: at, Parent: []deltaID{original.ID}, Payloads: []wireVariant{
		replayPayload(t, 6, []any{1, []byte{1}}, assistantKey, tool),
	}}
	state, err := replayHistory(append(batches, next), []deltaID{at})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := projectConversation(state); err == nil {
		t.Fatal("flattened a tool declaration into ordinary assistant text")
	}
	parent, _ := encodeWire([]any{[]any{}})
	next.Payloads = []wireVariant{replayPayload(t, 4, []any{1, []byte{55}}, parent)}
	state, err = replayHistory(append(batches, next), []deltaID{at})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := projectConversation(state); err == nil {
		t.Fatal("silently dropped inherited parent context")
	}
}

func TestConversationProjectionSkipsDeletedBoundaries(t *testing.T) {
	_, batches, _ := conversationFixture(t)
	remove := replayID(20, 1)
	batches = append(batches, historyDeltaBatch{
		ID: remove, Parent: []deltaID{batches[len(batches)-1].ID},
		Payloads: []wireVariant{replayEdit(t, "", 6, 9, 0)},
	})
	state, err := replayHistory(batches, []deltaID{remove})
	if err != nil {
		t.Fatal(err)
	}
	messages, err := projectConversation(state)
	if err != nil || len(messages) != 2 || messages[1].Role != "user" {
		t.Fatalf("deleted assistant boundary was still projected: %v", err)
	}
}

func TestConversationSeparatorsDoNotTrimEnclosedUserContent(t *testing.T) {
	_, batches, _ := conversationFixture(t)
	base, _ := batches[0].ID.wireValue()
	buffer := []any{0, []any{1, []byte{0}}}
	at := func(offset uint64) any {
		return []any{2, []any{buffer, []any{[]any{base, false}, offset}, uint64(1)}}
	}
	key, _ := encodeWire(at(0))
	// This user's two final newlines belong to the content, not the separator.
	user, _ := encodeWire([]any{uint64(0), []any{
		[]any{}, []any{}, []any{}, true, []any{[]any{at(0), at(6)}},
	}})
	last := replayID(20, 1)
	batches = append(batches, historyDeltaBatch{
		ID: last, Parent: []deltaID{batches[len(batches)-1].ID},
		Payloads: []wireVariant{
			replayPayload(t, 6, []any{1, []byte{1}}, key, user),
			replayEdit(t, "\n\n", 21, 0, 2),
		},
	})
	state, err := replayHistory(batches, []deltaID{last})
	if err != nil {
		t.Fatal(err)
	}
	messages, err := projectConversation(state)
	if err != nil || len(messages) != 3 || messages[0].Text != "USER\n\n" || messages[2].Text != "NEXT" {
		t.Fatalf("enclosed content or final stored separator was trimmed incorrectly: %v", err)
	}
}
