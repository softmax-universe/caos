package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"
	"unicode"

	"caos/w"
)

type contextBridgeState struct {
	Session     string
	Next        int
	Head        string
	Exporting   bool
	Inferred    bool
	Content     string
	Task        agentTask
	Publication *historyDeltaBatch
	ReplyBytes  int
	ToolCalls   int
}

func contextBridgeSession(snapshot historySnapshot) (string, error) {
	heads, err := snapshot.frontier()
	if err != nil {
		return "", err
	}
	version, err := historyVersionValue(heads)
	if err != nil {
		return "", err
	}
	timeline, err := snapshot.Timeline.wireValue()
	if err != nil {
		return "", err
	}
	identity, err := encodeWire([]any{snapshot.History[:], timeline, version})
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(identity)
	return "delta-bridge-" + hex.EncodeToString(digest[:16]), nil
}

func verifyExportedContext(data []byte, expected []agentMessage) error {
	var messages []struct {
		Role    string
		Content json.RawMessage
	}
	if json.Unmarshal(data, &messages) != nil || len(messages) != len(expected) {
		return errors.New("CAOS context does not contain the imported native messages")
	}
	for index, message := range messages {
		if message.Role != expected[index].Role {
			return errors.New("CAOS context changed a native message role")
		}
		if message.Role == "user" {
			var text string
			// Match the existing driver's user_text rule, not a second renderer.
			if json.Unmarshal(message.Content, &text) != nil ||
				text != strings.TrimRightFunc(expected[index].Text, unicode.IsSpace) {
				return errors.New("CAOS context changed the imported user text")
			}
			continue
		}
		var blocks []struct{ Type, Text string }
		if json.Unmarshal(message.Content, &blocks) != nil || len(blocks) != 1 ||
			blocks[0].Type != "text" || blocks[0].Text != expected[index].Text {
			return errors.New("CAOS context changed the imported assistant text")
		}
	}
	return nil
}

// Bridge approved native context through normal staged CAOS jobs. Only
// execute-turn requests inference/publication. No worker waits for another
// worker; every callback rechecks the authorized native frontier.
func runContextBridge(ctx context.Context, token string, actor uuid, snapshot historySnapshot, state *replayState, signingKey []byte) (string, bool, error) {
	execute := w.Read("/cas/args/op") == "execute-turn"
	bindingValues := []string{actor.String(), w.Read("/cas/args/thread-id"), w.Read("/cas/args/op"), w.Read("/cas/args/request-id")}
	if w.Exists("/cas/args/model") {
		bindingValues = append(bindingValues, w.Read("/cas/args/model"))
	}
	bindingData, err := json.Marshal(bindingValues)
	if err != nil {
		return "", false, err
	}
	binding := string(bindingData)
	var retained contextBridgeState
	if w.Exists("/cas/args/bridge-state") {
		retained, err = verifyBridgeState([]byte(w.Read("/cas/args/bridge-state")), signingKey, binding)
		if err != nil {
			return "", false, err
		}
		if retained.Publication != nil {
			if !execute {
				return "", false, errors.New("reply publication requires an execution continuation")
			}
			thread := threadID{History: snapshot.History, Timeline: snapshot.Timeline}
			if err := publishNativeBatch(ctx, token, actor, thread, retained.Session, *retained.Publication); err != nil {
				return "", false, err
			}
			return fmt.Sprintf("Executed the native Delta prompt through CAOS's existing agent driver and verified its reply in the same thread: %d reply bytes, %d tool calls. Model: %s.\n", retained.ReplyBytes, retained.ToolCalls, w.Read("/cas/args/model")), false, nil
		}
	}
	messages, err := projectConversation(state)
	if err != nil {
		return "", false, err
	}
	if len(messages) == 0 || messages[0].Role != "user" {
		return "", false, errors.New("context import requires an initial native user message")
	}
	for _, message := range messages {
		if !message.Complete || message.Role == "assistant" && strings.TrimSpace(message.Text) == "" {
			return "", false, errors.New("incomplete or empty assistant context import is not implemented")
		}
	}
	if execute && (messages[len(messages)-1].Role != "user" || !w.Exists("/cas/args/model") || strings.TrimSpace(w.Read("/cas/args/model")) == "") {
		return "", false, errors.New("execution requires a final native user prompt and an explicitly selected model")
	}
	session, err := contextBridgeSession(snapshot)
	if err != nil {
		return "", false, err
	}
	progress := contextBridgeState{Session: session}
	agent, err := newCaosAgent(w.Hash("/cas/args/agent-image"), w.Hash("/cas/args/result-publisher"))
	if err != nil {
		return "", false, err
	}
	if w.Exists("/cas/args/bridge-state") {
		progress = retained
		if progress.Session != session || progress.Next < 1 || progress.Next > len(messages) {
			return "", false, errors.New("native context changed or the bridge continuation is invalid")
		}
		const result = "/cas/args/result"
		if !w.Exists(result) {
			return "", false, errors.New("bridge continuation has no agent result")
		}
		expected, done, err := agent.poll(ctx, progress.Task)
		if err != nil || !done || w.Hash(result) != expected {
			return "", false, errors.New("callback result does not match its retained CAOS task")
		}
		w.Get(result)
		if !w.Exists(result + "/session") {
			return "", false, errors.New("CAOS context operation did not return a session")
		}
		head := w.Hash(result + "/session")
		if !isObjectHash(head) {
			return "", false, errors.New("invalid imported CAOS session head")
		}
		if progress.Exporting {
			if head != progress.Head {
				return "", false, errors.New("CAOS context export changed its pinned head")
			}
			data := []byte(w.Read(result + "/messages.json"))
			if execute {
				reply, calls, err := executedReply(data, messages)
				if err != nil {
					return "", false, err
				}
				// The request's timestamp is immutable across callback retries.
				// Never allocate different bytes for the same native batch ID.
				clock, err := time.Parse(time.RFC3339Nano, w.Read("/cas/args/request-id"))
				if err != nil || clock.UnixMilli() < 0 {
					return "", false, errors.New("execute-turn requires an RFC3339 request-id timestamp")
				}
				batch, err := appendMessageBatch(snapshot, state, actor, "assistant", reply, w.Read("/cas/args/model"), uint64(clock.UnixMilli()))
				if err != nil {
					return "", false, err
				}
				progress.Publication, progress.ReplyBytes, progress.ToolCalls = &batch, len(reply), calls
				// The memoized export is a barrier that retains the exact reply
				// batch in an immutable callback before its first send.
				return deferContextBridge(progress.Task, progress, signingKey, binding)
			}
			if err := verifyExportedContext(data, messages); err != nil {
				return "", false, err
			}
			return fmt.Sprintf("Imported and verified %d native Delta messages using CAOS's normal agent context renderer.\nNo inference was requested and no Delta turn was accepted or executed.\n", len(messages)), false, nil
		}
		progress.Head = head
		if progress.Inferred && progress.Content != bridgeContent(head) {
			return "", false, errors.New("agent changed workspace files; native file publication is not implemented")
		}
	}
	var task agentTask
	if progress.Next < len(messages) {
		locator := ""
		if progress.Next == 0 {
			w.Step("verify native workspace content in CAOS")
			locator, err = bridgeWorkspaceContent(ctx, token, actor, state)
			if err != nil {
				return "", false, err
			}
		}
		w.Step("prepare native message in the CAOS session")
		task, err = agent.prepareRecord(ctx, session, actor.String(), messages[progress.Next], locator, w.Read("/cas/args/request-id"))
		progress.Next++
	} else if execute && !progress.Inferred {
		progress.Content = bridgeContent(progress.Head)
		task, err = agent.prepareTurn(ctx, session, progress.Head, w.Read("/cas/args/model"),
			"You are the CAOS agent answering the latest user prompt from an authorized Delta thread. Use the existing file and tool capabilities to inspect its mounted source tree. Do not delegate to other agents. This bounded run is read-only: do not edit files or run external side effects. Answer the prompt with evidence from the actual files, not a canned response.", w.Read("/cas/args/request-id"))
		progress.Inferred = true
	} else {
		w.Step("export the pinned CAOS model context")
		task, err = agent.prepareContext(ctx, session, progress.Head, w.Read("/cas/args/request-id"))
		progress.Exporting = true
	}
	if err != nil {
		return "", false, err
	}
	return deferContextBridge(task, progress, signingKey, binding)
}

func bridgeWorkspaceContent(ctx context.Context, token string, actor uuid, state *replayState) (string, error) {
	references, err := threadWorkspaces(state)
	if err != nil {
		return "", err
	}
	if len(references) == 0 {
		return "", nil
	}
	if len(references) != 1 {
		return "", errors.New("multi-workspace session mounting is not implemented")
	}
	workspace, err := fetchInitialWorkspace(ctx, defaultRemoteURL, token, actor, references[0])
	if err != nil {
		return "", err
	}
	if workspace.Head == nil {
		return "", errors.New("workspace has no available CAOS base commit")
	}
	head := hex.EncodeToString(workspace.Head[:])
	w.Caos("resolve", head, ".", "/cas/native-workspace-tree")
	if w.Hash("/cas/native-workspace-tree") != hex.EncodeToString(workspace.Tree.Root[:]) {
		return "", errors.New("CAOS base commit differs from the native workspace tree")
	}
	return "git+caos://local?rev=" + head, nil
}

func deferContextBridge(task agentTask, progress contextBridgeState, signingKey []byte, binding string) (string, bool, error) {
	progress.Task = task
	data, err := signBridgeState(progress, signingKey, binding)
	if err != nil {
		return "", false, errors.New("could not encode bridge continuation")
	}
	directory, err := os.MkdirTemp("", "delta-context-bridge-")
	if err != nil {
		return "", false, errors.New("could not create bridge continuation")
	}
	path := directory + "/state.json"
	if err := os.WriteFile(path, data, 0600); err != nil {
		return "", false, errors.New("could not write bridge continuation")
	}
	w.Put(path, "/cas/context-bridge-state")
	own := w.Hash("/cas/args")
	args := []string{"curry", "--base:hash=" + own}
	for _, name := range []string{"bridge-state", "result", "error"} {
		if w.Exists("/cas/args/" + name) {
			args = append(args, "--unbind="+name)
		}
	}
	args = append(args, "--bridge-state:@=/cas/context-bridge-state")
	callback := w.Caos(args...)
	// The publisher preserves the driver's result. Its immutable completion ref
	// is also what the persistent listener will poll for an admitted turn.
	w.Caos("run-request-then", task.Task, "--then:hash="+callback)
	return "", true, nil
}
