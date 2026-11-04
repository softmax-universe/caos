package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

type runnerIdentity struct {
	Account        credentials `json:"account"`
	Actor          string      `json:"actor"`
	AllowedThreads []string    `json:"allowed_threads"`
}

func (identity runnerIdentity) authorizeHistory(thread string) (threadID, error) {
	for _, allowed := range identity.AllowedThreads {
		if thread == allowed {
			return parseThreadID(thread)
		}
	}
	return threadID{}, errors.New("thread is not authorized by the runner credential bundle")
}

func runWorker(ctx context.Context) (string, error) {
	if !w.Exists("/cas/args/request-id") || strings.TrimSpace(w.Read("/cas/args/request-id")) == "" {
		return "", errors.New("a fresh request-id is required")
	}
	op := "probe"
	if w.Exists("/cas/args/op") {
		op = w.Read("/cas/args/op")
	}
	if op != "probe" && op != "inspect-history" {
		return "", errors.New("unsupported Delta worker operation")
	}
	var name *string
	if w.Exists("/cas/args/registration-name") {
		if op != "probe" {
			return "", errors.New("registration-name is only valid for a probe")
		}
		value := strings.TrimSpace(w.Read("/cas/args/registration-name"))
		if value == "" {
			return "", errors.New("registration-name must not be empty")
		}
		name = &value
	}
	// This identity is sealed with the credentials. A caller cannot select an
	// existing desktop's actor or redirect its token to another service.
	var identity runnerIdentity
	data, err := os.ReadFile("/secret/delta-runner-credentials")
	if err != nil {
		return "", errors.New("could not read Delta runner credential bundle")
	}
	if err := json.Unmarshal(data, &identity); err != nil {
		return "", errors.New("invalid Delta runner credential bundle")
	}
	var thread threadID
	if op == "inspect-history" {
		if !w.Exists("/cas/args/thread-id") {
			return "", errors.New("thread-id is required for history inspection")
		}
		thread, err = identity.authorizeHistory(w.Read("/cas/args/thread-id"))
		if err != nil {
			return "", err
		}
	}
	actor, err := parseUUID(identity.Actor)
	if err != nil {
		return "", err
	}
	client := w.Check(newAuthClient(defaultCloudURL))
	token, err := client.deltaToken(ctx, identity.Account)
	if err != nil {
		return "", err
	}
	if op == "inspect-history" {
		snapshot, err := fetchHistorySnapshot(ctx, defaultRemoteURL, token, actor, thread)
		if err != nil {
			return "", err
		}
		// Thread content and pack metadata may be private. Do not publish them
		// into a CAOS result merely to establish that the transport works.
		return fmt.Sprintf("Read the authorized Delta test history from CAOS: %d inline batches, %d replication-pack references.\nHistory replay and agent-turn execution are not implemented.\n",
			len(snapshot.Download.Batches), len(snapshot.Download.ReplicationPacks)), nil
	}
	result, err := probeRemote(ctx, defaultRemoteURL, token, actor, name)
	if err != nil {
		return "", err
	}
	report := "Authenticated Delta protocol heartbeat accepted from a CAOS worker.\n"
	if result.RegistrationVerified {
		report += "Machine-runner registration and clearing both accepted.\n"
	}
	report += "This is a protocol probe, not an agent-turn executor.\n"
	return report, nil
}

func workerMain() {
	w.Step("Delta connector operation")
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	report, err := runWorker(ctx)
	dir := w.Check(os.MkdirTemp("", "delta-connector-"))
	if err != nil {
		// A diagnostic's negative result is a value, not a job failure that
		// would terminate the caller's entire agent turn.
		report = "Delta connector operation failed: " + err.Error() + "\n"
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("Delta connector operation failed\n"), 0600))
	}
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(report), 0600))
	w.Put(dir, "/cas/out")
}
