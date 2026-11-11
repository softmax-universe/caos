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
	"unicode"

	"caos/w"
)

type contextBridgeState struct {
	Session   string
	Next      int
	Head      string
	Exporting bool
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

// Bridge the approved native context through normal staged CAOS jobs. There is
// no inference here and no worker waits for another worker. Every callback
// rechecks the authorized native frontier before continuing its import.
func runContextBridge(ctx context.Context, token string, actor uuid, snapshot historySnapshot, state *replayState) (string, bool, error) {
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
	session, err := contextBridgeSession(snapshot)
	if err != nil {
		return "", false, err
	}
	progress := contextBridgeState{Session: session}
	if w.Exists("/cas/args/bridge-state") {
		if json.Unmarshal([]byte(w.Read("/cas/args/bridge-state")), &progress) != nil ||
			progress.Session != session || progress.Next < 1 || progress.Next > len(messages) {
			return "", false, errors.New("native context changed or the bridge continuation is invalid")
		}
		const result = "/cas/args/result"
		if !w.Exists(result) {
			return "", false, errors.New("bridge continuation has no agent result")
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
			if err := verifyExportedContext([]byte(w.Read(result+"/messages.json")), messages); err != nil {
				return "", false, err
			}
			return fmt.Sprintf("Imported and verified %d native Delta messages using CAOS's normal agent context renderer.\nNo inference was requested and no Delta turn was accepted or executed.\n", len(messages)), false, nil
		}
		progress.Head = head
	}
	agent, err := newCaosAgent(w.Hash("/cas/args/agent-image"), w.Hash("/cas/args/result-publisher"))
	if err != nil {
		return "", false, err
	}
	var task agentTask
	if progress.Next < len(messages) {
		locator := ""
		if progress.Next == 0 {
			locator, err = bridgeWorkspaceContent(ctx, token, actor, state)
			if err != nil {
				return "", false, err
			}
		}
		task, err = agent.prepareRecord(ctx, session, actor.String(), messages[progress.Next], locator, w.Read("/cas/args/request-id"))
		progress.Next++
	} else {
		task, err = agent.prepareContext(ctx, session, progress.Head, w.Read("/cas/args/request-id"))
		progress.Exporting = true
	}
	if err != nil {
		return "", false, err
	}
	return deferContextBridge(task, progress)
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

func deferContextBridge(task agentTask, progress contextBridgeState) (string, bool, error) {
	data, err := json.Marshal(progress)
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
