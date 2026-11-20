package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"testing"

	"github.com/vmihailenco/msgpack/v5"
)

func snapshotWithBatches(t *testing.T, batches []historyDeltaBatch, head deltaID) historySnapshot {
	t.Helper()
	timeline := replayID(0, 1)
	timelineValue, _ := timeline.wireValue()
	headValue, _ := head.wireValue()
	metadata, err := encodeWire([]any{timelineValue, []any{headValue}})
	if err != nil {
		t.Fatal(err)
	}
	return historySnapshot{Timeline: timeline, Timelines: []msgpack.RawMessage{metadata},
		Download: historyDownload{CoveredBatchCount: uint32(len(batches)), Batches: batches}}
}

func TestNativeAppendRoundTripAndRetainedPublication(t *testing.T) {
	state, batches, _ := conversationFixture(t)
	before, err := projectConversation(state)
	if err != nil {
		t.Fatal(err)
	}
	for _, role := range []string{"assistant", "user"} {
		snapshot := snapshotWithBatches(t, batches, batches[len(batches)-1].ID)
		text := "Real Unicode reply: α😀\n\n"
		batch, err := appendMessageBatch(snapshot, state, uuidFixture(20), role, text, "explicit-model", 123)
		if err != nil {
			t.Fatal(err)
		}
		updated := append(append([]historyDeltaBatch(nil), batches...), batch)
		after, err := replayHistory(updated, []deltaID{batch.ID})
		if err != nil {
			t.Fatal(err)
		}
		messages, err := projectConversation(after)
		if err != nil || len(messages) != len(before)+1 {
			t.Fatalf("native message did not replay: %v", err)
		}
		for index, message := range before {
			if messages[index] != message {
				t.Fatalf("append changed existing message %d: before=%+v after=%+v", index, message, messages[index])
			}
		}
		last := messages[len(messages)-1]
		if last.Role != role || !last.Complete || last.Text != text {
			t.Fatalf("native append lost text, role or completion: %+v", last)
		}
		complete := snapshotWithBatches(t, updated, batch.ID)
		found, err := retainedBatchPresent(complete, batch)
		if err != nil || !found {
			t.Fatal("accepted retained batch was not reconciled")
		}
		conflict := batch
		conflict.Payloads = append([]wireVariant(nil), batch.Payloads...)
		conflict.Payloads[0] = replayPayload(t, 5)
		if _, err := retainedBatchPresent(complete, conflict); err == nil {
			t.Fatal("conflicting reuse of a publication identity was accepted")
		}
		if bytes.Equal(batch.Payloads[0].Payload, []byte{}) {
			t.Fatal("no real text edit was formed")
		}
	}
}

func TestPromptSubmissionDeniesUnlistedThreadBeforeReadingCredentials(t *testing.T) {
	if err := submitPrompt(context.Background(), runnerIdentity{}, "unlisted", "", ""); err == nil {
		t.Fatal("unlisted prompt submission was accepted")
	}
}

func TestExecutedReplyUsesActualDriverToolContext(t *testing.T) {
	imported := []agentMessage{{Role: "user", Text: "inspect a real file", Complete: true}}
	context := []any{
		map[string]any{"role": "user", "content": imported[0].Text},
		map[string]any{"role": "assistant", "content": []any{
			map[string]any{"type": "tool_use", "id": "call", "name": "read", "input": map[string]any{"path": "SPEC.md"}},
		}},
		map[string]any{"role": "user", "content": []any{
			map[string]any{"type": "tool_result", "tool_use_id": "call", "content": "actual file content"},
		}},
		map[string]any{"role": "assistant", "content": []any{
			map[string]any{"type": "text", "text": "Evidence-based answer"},
		}},
	}
	data, _ := json.Marshal(context)
	reply, calls, err := executedReply(data, imported)
	if err != nil || reply != "Evidence-based answer" || calls != 1 {
		t.Fatalf("driver context projection failed: calls=%d error=%v", calls, err)
	}
	context[0] = map[string]any{"role": "user", "content": "different native prompt"}
	data, _ = json.Marshal(context)
	if _, _, err := executedReply(data, imported); err == nil {
		t.Fatal("reply for a different imported prompt was accepted")
	}
	context[0] = map[string]any{"role": "user", "content": imported[0].Text}
	for _, unfinished := range []any{
		map[string]any{"role": "user", "content": "not final"},
		map[string]any{"role": "assistant", "content": []any{}},
		context[1],
	} {
		context[len(context)-1] = unfinished
		data, _ = json.Marshal(context)
		if _, _, err := executedReply(data, imported); err == nil {
			t.Fatal("unfinished driver context became a completed native reply")
		}
	}
}

func TestNativeMessageMetadataMatchesIndependentCodecFixtures(t *testing.T) {
	state, batches, _ := conversationFixture(t)
	snapshot := snapshotWithBatches(t, batches, batches[len(batches)-1].ID)
	actor := "9201c410000102030405060708090a0b0c0d0e0f"
	at := func(offset string) string {
		return "92029392009201c401009292920b" + actor + "c2" + offset + "01"
	}
	expected := map[string]string{
		"user":      "92009891" + actor + "91b543414f532066697874757265206f70657261746f7290c29192" + at("02") + at("07") + "9090917b",
		"assistant": "92019b91" + actor + "9090917b90909090917b9090",
	}
	for _, role := range []string{"user", "assistant"} {
		batch, err := appendMessageBatch(snapshot, state, uuidFixture(0), role, "hello", "explicit-model", 123)
		if err != nil {
			t.Fatal(err)
		}
		fields, err := decodeFields(batch.Payloads[1].Payload, 3, 3)
		if err != nil {
			t.Fatal(err)
		}
		segment, err := decodeBytes(fields[2])
		if err != nil || hex.EncodeToString(segment) != expected[role] {
			t.Fatalf("%s differs from the independently native-decoded fixture: %x %v", role, segment, err)
		}
		if role == "assistant" {
			fields, _ = decodeFields(batch.Payloads[2].Payload, 3, 3)
			metadata, _ := decodeBytes(fields[2])
			if hex.EncodeToString(metadata) != "9391ae6578706c696369742d6d6f64656c9192a9616e7468726f706963ae6578706c696369742d6d6f64656c90" {
				t.Fatal("actual provider/model metadata differs from native fixture")
			}
		}
	}
}

func TestPublicationRequiresSelectedTimelineAndPermitsCompletedPromptRetry(t *testing.T) {
	state, batches, _ := conversationFixture(t)
	base := snapshotWithBatches(t, batches, batches[len(batches)-1].ID)
	prompt, err := appendMessageBatch(base, state, uuidFixture(0), "user", "real prompt", "", 123)
	if err != nil {
		t.Fatal(err)
	}
	withPrompt := append(append([]historyDeltaBatch(nil), batches...), prompt)
	offTimeline := snapshotWithBatches(t, withPrompt, batches[len(batches)-1].ID)
	if found, err := retainedBatchPresent(offTimeline, prompt); err != nil || found {
		t.Fatal("off-timeline stored batch became a visible success")
	}
	state, err = replayHistory(withPrompt, []deltaID{prompt.ID})
	if err != nil {
		t.Fatal(err)
	}
	promptSnapshot := snapshotWithBatches(t, withPrompt, prompt.ID)
	reply, err := appendMessageBatch(promptSnapshot, state, uuidFixture(0), "assistant", "actual reply", "explicit-model", 124)
	if err != nil {
		t.Fatal(err)
	}
	complete := snapshotWithBatches(t, append(withPrompt, reply), reply.ID)
	if found, err := retainedBatchPresent(complete, prompt); err != nil || !found {
		t.Fatalf("completed prompt did not reconcile after legitimate assistant linking: %v", err)
	}
	if found, err := retainedBatchPresent(complete, reply); err != nil || !found {
		t.Fatalf("actual reply text or previous messages failed readback: %v", err)
	}
}

func TestBridgeSignatureBindsStateKeyAndOperation(t *testing.T) {
	state := contextBridgeState{Session: "native-frontier", Head: "pinned-head", Next: 2}
	key, binding := []byte("private-connector-credential"), "actor/thread/model/request"
	data, err := signBridgeState(state, key, binding)
	if err != nil {
		t.Fatal(err)
	}
	got, err := verifyBridgeState(data, key, binding)
	if err != nil || got.Session != state.Session {
		t.Fatal("signed state failed roundtrip")
	}
	if _, err := verifyBridgeState(data, key, "other-actor/thread/model/request"); err == nil {
		t.Fatal("cross-operation replay accepted")
	}
	if _, err := verifyBridgeState(data, []byte("wrong-key"), binding); err == nil {
		t.Fatal("wrong identity accepted")
	}
	data = bytes.Replace(data, []byte("pinned-head"), []byte("forged-head"), 1)
	if _, err := verifyBridgeState(data, key, binding); err == nil {
		t.Fatal("modified state accepted")
	}
}
