package main

import (
	"context"
	"errors"
	"fmt"
	"os/exec"
	"strings"
)

var errAgentStartUncertain = errors.New("CAOS agent admission outcome is unknown; reconcile the retained task before retrying")

const agentResultPrefix = "refs/heads/caos-task-results/"

type agentTask struct {
	Request string
	Task    string
}

func isObjectHash(value string) bool {
	if len(value) != 40 {
		return false
	}
	for _, character := range value {
		if !(character >= '0' && character <= '9') && !(character >= 'a' && character <= 'f') {
			return false
		}
	}
	return true
}

func runCaosCommand(ctx context.Context, args ...string) (string, int, error) {
	if err := ctx.Err(); err != nil {
		return "", -1, err
	}
	command := exec.CommandContext(ctx, "caos", args...)
	output, err := command.Output()
	if err == nil {
		return strings.TrimSpace(string(output)), 0, nil
	}
	if ctx.Err() != nil {
		return "", -1, ctx.Err()
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) {
		if exit.ExitCode() == 2 && len(args) > 0 && args[0] == "ref-get" {
			return "", 2, nil
		}
		return "", exit.ExitCode(), fmt.Errorf("worker client command failed (exit %d)", exit.ExitCode())
	}
	// Neither argv nor stderr belongs in a diagnostic: they can contain a
	// private prompt, a source locator, or a server-side error with user data.
	return "", -1, errors.New("could not run the CAOS worker client")
}

// Reuse CAOS's normal agent driver, detached sub-runs and result publisher.
// The runner supplies these evaluated images. The server retains the launching
// job's secret context; the connector never receives its reader keys.
type caosAgent struct {
	image     string
	publisher string
	command   func(context.Context, ...string) (string, int, error)
}

func newCaosAgent(image, publisher string) (*caosAgent, error) {
	if !isObjectHash(image) || !isObjectHash(publisher) {
		return nil, errors.New("CAOS agent needs evaluated driver and result-publisher images")
	}
	return &caosAgent{image: image, publisher: publisher, command: runCaosCommand}, nil
}

func (agent *caosAgent) prepare(ctx context.Context, args ...string) (agentTask, error) {
	var task agentTask
	for _, arg := range args {
		if strings.HasPrefix(arg, "--base=") || strings.HasPrefix(arg, "--base:") {
			return task, errors.New("CAOS agent image cannot be overridden")
		}
	}
	argv := append([]string{"prepare-request", "--base:hash=" + agent.image}, args...)
	request, code, err := agent.command(ctx, argv...)
	if err != nil {
		return task, fmt.Errorf("could not prepare the CAOS agent request: %w", err)
	}
	if code != 0 || !isObjectHash(request) {
		return task, errors.New("could not prepare the CAOS agent request")
	}
	publisher, code, err := agent.command(ctx, "prepare-request",
		"--base:hash="+agent.publisher, "--subreq="+request, "--result-ref="+agentResultPrefix+request)
	if err != nil {
		return task, fmt.Errorf("could not prepare the CAOS result publisher: %w", err)
	}
	if code != 0 || !isObjectHash(publisher) {
		return task, errors.New("could not prepare the CAOS result publisher")
	}
	return agentTask{Request: request, Task: publisher}, nil
}

func (agent *caosAgent) prepareRecord(ctx context.Context, session, actor string, message agentMessage, contentLocator, requestID string) (agentTask, error) {
	if session == "" || actor == "" || message.ID == "" || requestID == "" {
		return agentTask{}, errors.New("incomplete CAOS context-recording request")
	}
	kind := "prompt"
	if message.Role == "assistant" && message.Complete {
		kind = "stop"
	} else if message.Role != "user" {
		return agentTask{}, errors.New("only completed native assistant messages can be imported")
	}
	args := []string{
		"--session=" + session, "--actor=" + actor, "--record=" + kind,
		"--message-id=" + message.ID, "--text=" + message.Text,
		"--request-id=" + requestID,
	}
	if contentLocator != "" {
		if kind != "prompt" {
			return agentTask{}, errors.New("only the first prompt can supply initial content")
		}
		args = append(args, "--content-locator="+contentLocator)
	}
	return agent.prepare(ctx, args...)
}

func (agent *caosAgent) prepareTurn(ctx context.Context, session, head, model, system, requestID string) (agentTask, error) {
	if !isObjectHash(head) || session == "" || model == "" || system == "" || requestID == "" {
		return agentTask{}, errors.New("incomplete CAOS agent turn request")
	}
	return agent.prepare(ctx,
		"--session="+session, "--head:commit="+head, "--model="+model,
		"--system="+system, "--request-id="+requestID)
}

func (agent *caosAgent) prepareContext(ctx context.Context, session, head, requestID string) (agentTask, error) {
	if session == "" || !isObjectHash(head) || requestID == "" {
		return agentTask{}, errors.New("context export requires a session and pinned head")
	}
	return agent.prepare(ctx, "--session="+session, "--head:commit="+head, "--export-context=1", "--request-id="+requestID)
}

// Preparation and admission are separate so the controller can retain the exact
// immutable task before any effects start. A retry must dispatch that same task.
func (agent *caosAgent) dispatch(ctx context.Context, task agentTask) error {
	if !isObjectHash(task.Request) || !isObjectHash(task.Task) {
		return errors.New("invalid retained CAOS agent task")
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	reply, code, err := agent.command(ctx, "sub-run", task.Task)
	if err != nil || code != 0 || reply != "request "+task.Task {
		return errAgentStartUncertain
	}
	return nil
}

func (agent *caosAgent) poll(ctx context.Context, task agentTask) (result string, done bool, err error) {
	if !isObjectHash(task.Request) || !isObjectHash(task.Task) {
		return "", false, errors.New("invalid retained CAOS agent task")
	}
	pollID, err := newUUID()
	if err != nil {
		return "", false, err
	}
	refPath := "/cas/delta-task-" + pollID.String()
	_, code, err := agent.command(ctx, "ref-get", agentResultPrefix+task.Request, refPath)
	if err != nil {
		return "", false, err
	}
	if code == 2 { // CAOS's documented absent-ref status, not a transport failure.
		return "", false, nil
	}
	if code != 0 {
		return "", false, errors.New("could not read the CAOS agent result")
	}
	commit, code, err := agent.command(ctx, "hash", refPath)
	if err != nil || code != 0 || !isObjectHash(commit) {
		return "", false, errors.New("invalid CAOS task result commit")
	}
	resultPath := "/cas/delta-task-result-" + pollID.String()
	_, code, err = agent.command(ctx, "resolve", commit, "result", resultPath)
	if err != nil || code != 0 {
		return "", false, errors.New("could not resolve the CAOS task result")
	}
	kind, code, err := agent.command(ctx, "kind", resultPath)
	if err != nil || code != 0 || kind != "tree" {
		return "", false, errors.New("CAOS agent returned a non-tree result")
	}
	result, code, err = agent.command(ctx, "hash", resultPath)
	if err != nil || code != 0 || !isObjectHash(result) {
		return "", false, errors.New("invalid CAOS agent result identity")
	}
	return result, true, nil
}
