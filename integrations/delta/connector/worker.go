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

func workerProbe() {
	w.Step("Delta protocol probe")
	w.True(w.Exists("/cas/args/request-id"), "a fresh request-id is required")
	w.True(strings.TrimSpace(w.Read("/cas/args/request-id")) != "", "request-id must not be empty")
	if w.Exists("/cas/args/op") {
		w.True(w.Read("/cas/args/op") == "probe", "only the probe operation is implemented")
	}
	// This identity is sealed with the credentials. A caller cannot select an
	// existing desktop's actor or redirect its token to another service.
	var identity runnerIdentity
	data := w.Check(os.ReadFile("/secret/delta-runner-credentials"))
	if err := json.Unmarshal(data, &identity); err != nil {
		panic(errors.New("invalid Delta runner credential bundle"))
	}
	actor := w.Check(parseUUID(identity.Actor))
	client := w.Check(newAuthClient(defaultCloudURL))
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	token := w.Check(client.deltaToken(ctx, identity.Account))
	var name *string
	if w.Exists("/cas/args/registration-name") {
		value := strings.TrimSpace(w.Read("/cas/args/registration-name"))
		w.True(value != "", "registration-name must not be empty")
		name = &value
	}
	result := w.Check(probeRemote(ctx, defaultRemoteURL, token, actor, name))
	dir := w.Check(os.MkdirTemp("", "delta-probe-"))
	report := "Authenticated Delta protocol heartbeat accepted from a CAOS worker.\n"
	if result.RegistrationVerified {
		report += "Machine-runner registration and clearing both accepted.\n"
	}
	report += "This is a protocol probe, not an agent-turn executor.\n"
	w.Must(os.WriteFile(filepath.Join(dir, "stdout"), []byte(report), 0600))
	w.Put(dir, "/cas/out")
}
