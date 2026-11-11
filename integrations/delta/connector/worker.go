package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"time"

	"caos/w"
)

type runnerIdentity struct {
	Account credentials `json:"account"`
	Actor   string      `json:"actor"`
}

func runWorkerProbe(ctx context.Context) (string, error) {
	if !w.Exists("/cas/args/request-id") || strings.TrimSpace(w.Read("/cas/args/request-id")) == "" {
		return "", errors.New("a fresh request-id is required")
	}
	if w.Exists("/cas/args/op") {
		if w.Read("/cas/args/op") != "probe" {
			return "", errors.New("unsupported Delta worker operation")
		}
	}
	var name *string
	if w.Exists("/cas/args/registration-name") {
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
	actor, err := parseUUID(identity.Actor)
	if err != nil {
		return "", err
	}
	client := w.Check(newAuthClient(defaultCloudURL))
	token, err := client.deltaToken(ctx, identity.Account)
	if err != nil {
		return "", err
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

func workerProbe() {
	w.Step("Delta protocol probe")
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	report, err := runWorkerProbe(ctx)
	dir := w.Check(os.MkdirTemp("", "delta-probe-"))
	if err != nil {
		// A diagnostic's negative result is a value, not a job failure that
		// would terminate the caller's entire agent turn.
		report = "Delta protocol probe failed: " + err.Error() + "\n"
		w.Must(os.WriteFile(filepath.Join(dir, "failed"), []byte("Delta probe failed\n"), 0600))
	}
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(report), 0600))
	w.Put(dir, "/cas/out")
}
