package main

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"

	"caos/w"
)

func bridgeContent(head string) string {
	const root = "/cas/delta-session-root"
	w.Caos("resolve", head, ".", root)
	w.Get(root)
	directory := w.Check(os.MkdirTemp("", "delta-session-content-"))
	for _, entry := range w.Check(os.ReadDir(root)) {
		if entry.Name() != ".caos" {
			w.Must(os.Symlink(filepath.Join(root, entry.Name()), filepath.Join(directory, entry.Name())))
		}
	}
	w.Put(directory, "/cas/delta-session-content")
	return w.Hash("/cas/delta-session-content")
}

// Use the driver's normal context renderer, including real tool-use/results.
// Only the final assistant's text becomes the native reply. The full tool trace
// remains in the ordinary CAOS session, not a second transcript implementation.
func executedReply(data []byte, imported []agentMessage) (string, int, error) {
	var messages []struct {
		Role    string          `json:"role"`
		Content json.RawMessage `json:"content"`
	}
	if json.Unmarshal(data, &messages) != nil || len(messages) <= len(imported) {
		return "", 0, errors.New("agent context contains no new reply")
	}
	prefix, err := json.Marshal(messages[:len(imported)])
	if err != nil || verifyExportedContext(prefix, imported) != nil {
		return "", 0, errors.New("agent changed its imported native context")
	}
	calls := 0
	lastText := []string{}
	for index, message := range messages[len(imported):] {
		last := index == len(messages)-len(imported)-1
		if last && message.Role != "assistant" {
			return "", 0, errors.New("agent did not finish with an assistant response")
		}
		if message.Role != "assistant" {
			continue
		}
		var blocks []struct{ Type, Text string }
		if json.Unmarshal(message.Content, &blocks) != nil {
			return "", 0, errors.New("invalid agent assistant content")
		}
		for _, block := range blocks {
			if block.Type == "tool_use" {
				calls++
				if last {
					return "", 0, errors.New("agent reply still has pending tool calls")
				}
			}
			if last && block.Type == "text" {
				lastText = append(lastText, block.Text)
			}
		}
	}
	reply := strings.Join(lastText, "\n\n")
	if strings.TrimSpace(reply) == "" {
		return "", 0, errors.New("agent returned no final reply text")
	}
	return reply, calls, nil
}
