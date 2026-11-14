package main

import (
	"context"
	"crypto/sha256"
	"errors"
	"os"
	"time"
)

type promptWritePlan struct {
	Thread   string
	Actor    string
	Frontier string
	Digest   [32]byte
	Batch    historyDeltaBatch
}

// An explicit, one-shot operator submission to a sealed allowlisted thread.
// It does not ask an existing Delta agent to run anything.
func submitPrompt(ctx context.Context, identity runnerIdentity, threadName, textFile, planFile string) error {
	thread, err := identity.authorizeHistory(threadName)
	if err != nil {
		return err
	}
	actor, err := parseUUID(identity.Actor)
	if err != nil {
		return err
	}
	text, err := os.ReadFile(textFile)
	if err != nil || len(text) > 64<<10 || len(text) == 0 {
		return errors.New("a prompt file of 1–65536 bytes is required")
	}
	if planFile == "" {
		return errors.New("a private retained publication plan is required")
	}
	client, err := newAuthClient(defaultCloudURL)
	if err != nil {
		return err
	}
	token, err := client.deltaToken(ctx, identity.Account)
	if err != nil {
		return err
	}
	plan := promptWritePlan{Thread: threadName, Actor: identity.Actor, Digest: sha256.Sum256(text)}
	if _, err := os.Lstat(planFile); os.IsNotExist(err) {
		snapshot, err := fetchHistorySnapshot(ctx, defaultRemoteURL, token, actor, thread)
		if err != nil {
			return err
		}
		state, err := replaySnapshot(snapshot)
		if err != nil {
			return err
		}
		messages, err := projectConversation(state)
		if err != nil || len(messages) == 0 || messages[len(messages)-1].Role != "assistant" || !messages[len(messages)-1].Complete {
			return errors.New("prompt submission requires a completed native assistant turn")
		}
		plan.Frontier, err = contextBridgeSession(snapshot)
		if err != nil {
			return err
		}
		plan.Batch, err = appendMessageBatch(snapshot, state, actor, "user", string(text), "", uint64(time.Now().UnixMilli()))
		if err != nil {
			return err
		}
		if err := writePrivateJSON(planFile, plan); err != nil {
			return err
		}
	} else {
		if err := readPrivateJSON(planFile, &plan); err != nil {
			return err
		}
		if plan.Thread != threadName || plan.Actor != identity.Actor || plan.Digest != sha256.Sum256(text) {
			return errors.New("retained prompt plan does not match this submission")
		}
	}
	return publishNativeBatch(ctx, token, actor, thread, plan.Frontier, plan.Batch)
}
