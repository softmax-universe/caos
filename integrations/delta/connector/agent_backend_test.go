package main

import (
	"context"
	"errors"
	"reflect"
	"strings"
	"testing"
)

func TestCaosAgentPreparesAndDispatchesTheExistingDriver(t *testing.T) {
	image, publisher := strings.Repeat("a", 40), strings.Repeat("b", 40)
	request, task := strings.Repeat("c", 40), strings.Repeat("d", 40)
	agent, err := newCaosAgent(image, publisher)
	if err != nil {
		t.Fatal(err)
	}
	var calls [][]string
	agent.command = func(_ context.Context, args ...string) (string, int, error) {
		calls = append(calls, append([]string(nil), args...))
		switch len(calls) {
		case 1:
			return request, 0, nil
		case 2:
			return task, 0, nil
		case 3:
			return "request " + task, 0, nil
		default:
			t.Fatal("unexpected CAOS command")
			return "", 1, nil
		}
	}
	message := agentMessage{ID: strings.Repeat("1", 32), Role: "user", Text: "a prompt\n"}
	prepared, err := agent.prepareRecord(context.Background(), "session", "actor", message, "git+caos://local?rev="+request, "record-attempt")
	if err != nil || prepared != (agentTask{Request: request, Task: task}) {
		t.Fatalf("record was not prepared: %v", err)
	}
	if len(calls) != 2 {
		t.Fatal("preparation dispatched effects before its identity could be retained")
	}
	expected := []string{
		"prepare-request", "--base:hash=" + image,
		"--session=session", "--actor=actor", "--record=prompt",
		"--message-id=" + message.ID, "--text=" + message.Text,
		"--request-id=record-attempt",
		"--content-locator=git+caos://local?rev=" + request,
	}
	if !reflect.DeepEqual(calls[0], expected) || !reflect.DeepEqual(calls[1], []string{
		"prepare-request", "--base:hash=" + publisher, "--subreq=" + request,
		"--result-ref=" + agentResultPrefix + request,
	}) {
		t.Fatal("prepared a different driver, message or result publisher")
	}
	if err := agent.dispatch(context.Background(), prepared); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(calls[2], []string{"sub-run", task}) {
		t.Fatal("did not use the native detached sub-run mechanism")
	}
}

func TestCaosAgentDoesNotRelabelMessagesOrModelConfiguration(t *testing.T) {
	agent, _ := newCaosAgent(strings.Repeat("a", 40), strings.Repeat("b", 40))
	var prepared [][]string
	agent.command = func(_ context.Context, args ...string) (string, int, error) {
		prepared = append(prepared, append([]string(nil), args...))
		return strings.Repeat("c", 40), 0, nil
	}
	message := agentMessage{ID: strings.Repeat("1", 32), Role: "assistant", Text: "answer", Complete: true}
	if _, err := agent.prepareRecord(context.Background(), "s", "a", message, "", "record-attempt"); err != nil {
		t.Fatal(err)
	}
	if !containsArgument(prepared[0], "--record=stop") {
		t.Fatal("assistant text was relabeled as a user prompt")
	}
	head := strings.Repeat("d", 40)
	if _, err := agent.prepareTurn(context.Background(), "s", head, "configured-model", "system", "request"); err != nil {
		t.Fatal(err)
	}
	for _, arg := range []string{"--head:hash=" + head, "--model=configured-model", "--system=system", "--request-id=request"} {
		if !containsArgument(prepared[2], arg) {
			t.Fatal("model turn lost its pinned head or explicit configuration")
		}
	}
	if _, err := agent.prepareContext(context.Background(), "s", head, "export-attempt"); err != nil {
		t.Fatal(err)
	}
	if !containsArgument(prepared[4], "--export-context=1") {
		t.Fatal("did not use the existing driver's context renderer")
	}
	if _, err := agent.prepareRecord(context.Background(), "s", "a", agentMessage{Role: "assistant", ID: message.ID}, "", "record-attempt"); err == nil {
		t.Fatal("imported an incomplete assistant turn")
	}
	if _, err := agent.prepare(context.Background(), "--base:hash="+head); err == nil {
		t.Fatal("allowed the configured driver to be overridden")
	}
}

func containsArgument(args []string, expected string) bool {
	for _, arg := range args {
		if arg == expected {
			return true
		}
	}
	return false
}

func TestCaosAgentPollDistinguishesAbsentFromBroken(t *testing.T) {
	agent, _ := newCaosAgent(strings.Repeat("a", 40), strings.Repeat("b", 40))
	task := agentTask{Request: strings.Repeat("c", 40), Task: strings.Repeat("d", 40)}
	result, commit := strings.Repeat("e", 40), strings.Repeat("f", 40)
	code := 2
	agent.command = func(_ context.Context, args ...string) (string, int, error) {
		switch args[0] {
		case "ref-get":
			if args[1] != agentResultPrefix+task.Request || len(args) != 3 {
				t.Fatal("polled the wrong result ref")
			}
			return "", code, nil
		case "hash":
			if strings.Contains(args[1], "task-result-") {
				return result, 0, nil
			}
			return commit, 0, nil
		case "resolve":
			if args[1] != commit || args[2] != "result" {
				t.Fatal("resolved a different task result")
			}
			return "", 0, nil
		case "kind":
			return "tree", 0, nil
		default:
			t.Fatal("unexpected poll command")
			return "", 1, nil
		}
	}
	if _, done, err := agent.poll(context.Background(), task); err != nil || done {
		t.Fatal("absent result was treated as complete or broken")
	}
	code = 1
	if _, _, err := agent.poll(context.Background(), task); err == nil {
		t.Fatal("transport failure was treated as an absent result")
	}
	code = 0
	if got, done, err := agent.poll(context.Background(), task); err != nil || !done || got != result {
		t.Fatalf("did not resolve the completed result tree: %v", err)
	}
}

func TestCaosAgentRetainsUncertainAdmissionIdentity(t *testing.T) {
	agent, _ := newCaosAgent(strings.Repeat("a", 40), strings.Repeat("b", 40))
	task := agentTask{Request: strings.Repeat("c", 40), Task: strings.Repeat("d", 40)}
	calls := 0
	agent.command = func(_ context.Context, args ...string) (string, int, error) {
		calls++
		return "", 1, nil
	}
	if err := agent.dispatch(context.Background(), task); !errors.Is(err, errAgentStartUncertain) {
		t.Fatal("failed admission was assumed not to have executed")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := agent.dispatch(ctx, task); !errors.Is(err, context.Canceled) || calls != 1 {
		t.Fatal("pre-canceled dispatch started work")
	}
}

func TestCaosAgentPreparationPreservesDeadlineErrors(t *testing.T) {
	agent, _ := newCaosAgent(strings.Repeat("a", 40), strings.Repeat("b", 40))
	agent.command = func(context.Context, ...string) (string, int, error) {
		return "", -1, context.DeadlineExceeded
	}
	message := agentMessage{ID: strings.Repeat("1", 32), Role: "user", Complete: true}
	if _, err := agent.prepareRecord(context.Background(), "s", "a", message, "", "attempt"); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatal("request preparation hid its actual timeout")
	}
}

func TestCaosFailureClassificationDoesNotExposePrivateStderr(t *testing.T) {
	for _, test := range []struct{ stderr, category string }{
		{"caos: usage: private-prompt", "does not support"},
		{"private-prompt already bound", "already bound"},
		{"private-prompt not found", "object was not found"},
		{"private-prompt: No such file or directory", "path is missing"},
		{"private-prompt: Permission denied", "denied access"},
		{"private-prompt private-locator private-token", "command failed"},
	} {
		err := classifyCaosFailure(test.stderr, 1)
		if strings.Contains(err.Error(), "private-") || !strings.Contains(err.Error(), test.category) ||
			!strings.Contains(err.Error(), "exit 1") {
			t.Fatalf("unsafe or uninformative CLI error: %v", err)
		}
	}
}
