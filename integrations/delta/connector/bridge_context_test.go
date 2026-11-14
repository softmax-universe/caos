package main

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func TestContextBridgeVerifiesTheNormalDriverRepresentation(t *testing.T) {
	expected := []agentMessage{
		{ID: strings.Repeat("1", 32), Role: "user", Text: "  user text \n", Complete: true},
		{ID: strings.Repeat("2", 32), Role: "assistant", Text: "  answer\n", Complete: true},
	}
	correct := `[{"role":"user","content":"  user text"},{"role":"assistant","content":[{"type":"text","text":"  answer\n"}]}]`
	if err := verifyExportedContext([]byte(correct), expected); err != nil {
		t.Fatal(err)
	}
	for _, data := range []string{
		`null`,
		`[]`,
		`[{"role":"user","content":"  user text"}]`,
		strings.Replace(correct, `"role":"assistant"`, `"role":"user"`, 1),
		strings.Replace(correct, `"  user text"`, `"user text"`, 1),
		strings.Replace(correct, `"type":"text"`, `"type":"tool_use"`, 1),
		strings.Replace(correct, `"  answer\n"`, `"different"`, 1),
	} {
		if err := verifyExportedContext([]byte(data), expected); err == nil {
			t.Fatal("accepted a lossy or relabeled model context")
		}
	}
}

func TestContextBridgeIdentityPinsTheNativeHistoryAndTimelineFrontier(t *testing.T) {
	head, timeline := replayID(7, 1), replayID(1, 1)
	makeSnapshot := func(timeline, head deltaID) historySnapshot {
		timelineWire, _ := timeline.wireValue()
		headWire, _ := head.wireValue()
		metadata, _ := encodeWire([]any{timelineWire, []any{headWire}})
		return historySnapshot{History: uuidFixture(1), Timeline: timeline, Timelines: []msgpack.RawMessage{metadata}}
	}
	snapshot := makeSnapshot(timeline, head)
	first, err := contextBridgeSession(snapshot)
	if err != nil || !strings.HasPrefix(first, "delta-bridge-") {
		t.Fatal("could not identify the pinned native context")
	}
	repeated, err := contextBridgeSession(snapshot)
	if err != nil || repeated != first {
		t.Fatal("the same context changed its session identity")
	}
	other := makeSnapshot(timeline, replayID(8, 1))
	changed, err := contextBridgeSession(other)
	if err != nil || changed == first {
		t.Fatal("an edited native context reused the old session")
	}
	other = makeSnapshot(replayID(2, 1), head)
	changed, err = contextBridgeSession(other)
	if err != nil || changed == first {
		t.Fatal("another native timeline reused the old session")
	}
	other = snapshot
	other.History = uuidFixture(2)
	changed, err = contextBridgeSession(other)
	if err != nil || changed == first {
		t.Fatal("another history reused the old session")
	}
	snapshot.Timelines = nil
	if _, err := contextBridgeSession(snapshot); err == nil {
		t.Fatal("bridge identity did not require a known native frontier")
	}
	// Import continuations contain counters and object identities, never
	// native metadata that can hold inference vouchers. A final publication
	// continuation additionally retains the newly generated reply batch.
	state, err := json.Marshal(contextBridgeState{Session: first, Next: 2, Head: strings.Repeat("a", 40), Exporting: true})
	if err != nil || strings.Contains(string(state), "voucher") {
		t.Fatal("bridge continuation retained conversation metadata")
	}
}
