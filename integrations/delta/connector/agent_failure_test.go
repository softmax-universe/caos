package main

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestAgentFailureRecognizesOnlyExactRedactedCreditDenial(t *testing.T) {
	const body = `{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"private-request-id"}`
	const prefix = "llm-step: POST https://api.anthropic.com/v1/messages: 400 Bad Request: "
	err := agentFailure("worker failed\n" + prefix + body + "\n")
	if !errors.Is(err, errInferenceCredit) || strings.Contains(err.Error(), "private-") {
		t.Fatal("actual provider credit denial was hidden or leaked its payload")
	}
	for _, text := range []string{
		body,
		"private prompt: " + prefix + body,
		strings.Replace(prefix+body, "api.anthropic.com", "other.example", 1),
		prefix + `{"type":"error","error":{"type":"invalid_request_error","message":"private-user-content"}}`,
		"arbitrary private-provider-token",
	} {
		err := agentFailure(text)
		if errors.Is(err, errInferenceCredit) || strings.Contains(err.Error(), "private-") {
			t.Fatalf("untrusted failure was exposed or misclassified: %v", err)
		}
	}
}

func TestRetainedAgentFailureAcceptsPublishersNewlineStatus(t *testing.T) {
	dir := t.TempDir()
	// Only materialization is stubbed. The real result-file reading, status
	// normalization, bound and provider-envelope classification run unchanged.
	bin := t.TempDir()
	shell, err := exec.LookPath("sh")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(bin, "caos"), []byte("#!"+shell+"\nexit 0\n"), 0700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin)
	if err := os.WriteFile(filepath.Join(dir, "status"), []byte("failed\n"), 0600); err != nil {
		t.Fatal(err)
	}
	message := `worker failed
llm-step: POST https://api.anthropic.com/v1/messages: 400 Bad Request: {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}`
	if err := os.WriteFile(filepath.Join(dir, "error"), []byte(message), 0600); err != nil {
		t.Fatal(err)
	}
	if err := agentResultFailure(dir); !errors.Is(err, errInferenceCredit) {
		t.Fatalf("actual publisher status did not expose the redacted credit cause: %v", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "status"), []byte("pending\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := agentResultFailure(dir); errors.Is(err, errInferenceCredit) {
		t.Fatal("nonterminal result was classified as an inference denial")
	}
}
