package main

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"strings"

	"caos/w"
)

var errInferenceCredit = errors.New("Anthropic inference denied: the approved API credential has insufficient credit")

func agentResultFailure(result string) error {
	if !w.Exists(result+"/status") || strings.TrimSpace(w.Read(result+"/status")) != "failed" || !w.Exists(result+"/error") {
		return errors.New("CAOS context operation did not return a session")
	}
	w.Get(result + "/error")
	file, err := os.Open(result + "/error")
	if err != nil {
		return errors.New("could not read the retained CAOS agent failure")
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 16<<10))
	if err != nil {
		return errors.New("could not read the retained CAOS agent failure")
	}
	return agentFailure(string(data))
}

// A task result is authenticated before this is called. Only recognize the
// driver's exact provider failure envelope; never print arbitrary stderr or
// provider bodies (which can contain private user data).
func agentFailure(text string) error {
	const prefix = `llm-step: POST https://api.anthropic.com/v1/messages: 400 Bad Request: `
	for _, line := range strings.Split(text, "\n") {
		if !strings.HasPrefix(line, prefix) {
			continue
		}
		var body struct {
			Type  string `json:"type"`
			Error struct {
				Type    string `json:"type"`
				Message string `json:"message"`
			} `json:"error"`
		}
		if json.Unmarshal([]byte(strings.TrimPrefix(line, prefix)), &body) == nil &&
			body.Type == "error" && body.Error.Type == "invalid_request_error" &&
			body.Error.Message == "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits." {
			return errInferenceCredit
		}
	}
	return errors.New("CAOS agent task failed; its retained task output contains private diagnostics")
}
