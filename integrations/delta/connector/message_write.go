package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"strings"
	"unicode/utf8"

	"github.com/vmihailenco/msgpack/v5"
)

func nativeAnchor(insertion deltaID, offset int) ([]byte, error) {
	id, err := insertion.wireValue()
	if err != nil {
		return nil, err
	}
	buffer := []any{uint8(0), []any{uint8(1), []byte{0}}}
	// Native Right anchors precede the addressed byte; Left follows it.
	return encodeWire([]any{uint8(2), []any{buffer, []any{[]any{id, false}, uint64(offset)}, uint8(1)}})
}

func nativePayload(tag uint64, value any) (wireVariant, error) {
	data, err := encodeWire(value)
	return wireVariant{Tag: tag, Payload: data}, err
}

// Append only: never edit an existing message or its sealed ranges. The caller
// retains this exact batch before sending and checks its synchronized frontier.
func appendMessageBatch(snapshot historySnapshot, state *replayState, actor uuid, role, text, model string, now uint64) (historyDeltaBatch, error) {
	var empty historyDeltaBatch
	if strings.TrimSpace(text) == "" || !utf8.ValidString(text) || len(text) > 1<<20 {
		return empty, errors.New("invalid native message text")
	}
	if role != "user" && role != "assistant" {
		return empty, errors.New("unsupported native message role")
	}
	if state.Buffers[fieldObject(0)] == nil {
		return empty, errors.New("native thread has no message buffer")
	}
	current := state.Buffers[fieldObject(0)].stringAt(state.basis)
	prefix := ""
	if current != "" && !strings.HasSuffix(current, "\n\n") {
		prefix = "\n\n"
	}
	inserted := prefix + text + "\n\n"
	count := 2
	if role == "assistant" {
		if model == "" {
			return empty, errors.New("assistant reply requires its actual model identity")
		}
		count += 2 // Actor identity and the user's submitted-assistant link.
	}
	batch, err := newHistoryBatch(snapshot, actor, make([]wireVariant, count))
	if err != nil {
		return empty, err
	}
	insertion, err := payloadID(batch, 0)
	if err != nil {
		return empty, err
	}
	start, err := nativeAnchor(insertion, len(prefix))
	if err != nil {
		return empty, err
	}
	end, err := nativeAnchor(insertion, len(prefix)+len(text))
	if err != nil {
		return empty, err
	}
	// ScratchBufferEdit and MapInsert contain the same typed values the replay
	// engine and public native reference consume.
	batch.Payloads[0], err = nativePayload(2, []any{
		[]any{uint8(1), []byte{0}},
		[]any{inserted, uint64(len(current)), uint64(0), uint64(len(inserted))},
	})
	if err != nil {
		return empty, err
	}
	option := func(value any) []any { return []any{value} }
	none := []any{}
	actorValue := []any{uint8(1), actor[:]}
	var segment any
	if role == "user" {
		segment = []any{uint8(0), []any{option(actorValue), option("CAOS fixture operator"), none,
			false, option([]any{msgpack.RawMessage(start), msgpack.RawMessage(end)}), none, none, option(now)}}
	} else {
		segment = []any{uint8(1), []any{option(actorValue), none, none, option(now),
			none, none, none, none, option(now), []any{}, []any{}}}
		metadata, err := encodeWire([]any{option(model), option([]any{"anthropic", model}), none})
		if err != nil {
			return empty, err
		}
		key, err := encodeWire(actorValue)
		if err != nil {
			return empty, err
		}
		batch.Payloads[2], err = nativePayload(6, []any{[]any{uint8(1), []byte{12}}, key, metadata})
		if err != nil {
			return empty, err
		}
		// Preserve the user's metadata while linking this actual assistant turn.
		// Without that link a native client can still display the prompt as a draft.
		values := state.Maps[fieldObject(1)]
		lastOffset := -1
		var lastKey []byte
		var lastUser wireVariant
		for key := range values.entries {
			value := values.get(key, state.basis)
			if !value.Present {
				continue
			}
			anchor, err := decodeScratchAnchor([]byte(key))
			if err != nil {
				return empty, err
			}
			offset, err := state.resolveVisibleAnchor(anchor)
			if err != nil {
				continue
			}
			segment, err := decodeVariant(value.Bytes)
			if err != nil {
				return empty, err
			}
			if offset > lastOffset {
				lastOffset, lastKey, lastUser = offset, []byte(key), segment
			}
		}
		if lastOffset < 0 || lastUser.Tag != 0 {
			return empty, errors.New("assistant publication requires a final native user message")
		}
		fields, err := decodeFields(lastUser.Payload, 0, 8)
		if err != nil {
			return empty, err
		}
		defaults := []any{none, none, none, false, none, none, none, none}
		for index := len(fields); index < len(defaults); index++ {
			data, err := encodeWire(defaults[index])
			if err != nil {
				return empty, err
			}
			fields = append(fields, data)
		}
		fields[5], err = encodeWire(option(msgpack.RawMessage(start)))
		if err != nil {
			return empty, err
		}
		updated, err := encodeWire([]any{uint8(0), fields})
		if err != nil {
			return empty, err
		}
		batch.Payloads[3], err = nativePayload(6, []any{[]any{uint8(1), []byte{1}}, lastKey, updated})
		if err != nil {
			return empty, err
		}
	}
	value, err := encodeWire(segment)
	if err != nil {
		return empty, err
	}
	batch.Payloads[1], err = nativePayload(6, []any{[]any{uint8(1), []byte{1}}, start, value})
	return batch, err
}

func sameHistoryBatch(a, b historyDeltaBatch) bool {
	av, aerr := historyBatchValue(a)
	bv, berr := historyBatchValue(b)
	if aerr != nil || berr != nil {
		return false
	}
	ab, aerr := encodeWire(av)
	bb, berr := encodeWire(bv)
	return aerr == nil && berr == nil && bytes.Equal(ab, bb)
}

// A batch accepted before a disconnect is reconciled by identity, not reallocated.
func retainedBatchPresent(snapshot historySnapshot, batch historyDeltaBatch) (bool, error) {
	heads, err := snapshot.frontier()
	if err != nil {
		return false, err
	}
	plan, _, err := planHistory(snapshot.Download.Batches, heads)
	if err != nil {
		return false, err
	}
	for _, known := range plan {
		if known.Batch.ID == batch.ID {
			if !sameHistoryBatch(known.Batch, batch) {
				return false, errors.New("native publication identity has conflicting contents")
			}
			state, err := replaySnapshot(snapshot)
			if err != nil {
				return false, err
			}
			// Verify the batch's actual message entry and visible text, not
			// merely its storage somewhere in this history.
			keyFields, err := decodeFields(batch.Payloads[1].Payload, 3, 3)
			if err != nil {
				return false, err
			}
			key, err := decodeBytes(keyFields[1])
			if err != nil {
				return false, err
			}
			expected, err := decodeBytes(keyFields[2])
			if err != nil {
				return false, err
			}
			values := state.Maps[fieldObject(1)]
			if values == nil {
				return false, errors.New("native reply metadata is not visible at the selected frontier")
			}
			value := values.get(string(key), state.basis)
			if !value.Present || !samePublishedSegment(expected, value.Bytes) {
				return false, errors.New("native message identity is not visible at the selected frontier")
			}
			anchor, err := decodeScratchAnchor(key)
			if err != nil {
				return false, err
			}
			if _, err := state.resolveVisibleAnchor(anchor); err != nil {
				return false, err
			}
			editFields, err := decodeFields(batch.Payloads[0].Payload, 2, 2)
			if err != nil {
				return false, err
			}
			edit, err := decodeTextEdit(editFields[1])
			if err != nil || anchor.Offset > uint64(len(edit.Text)) {
				return false, errors.New("invalid retained message text")
			}
			text := strings.TrimSuffix(edit.Text[int(anchor.Offset):], "\n\n")
			messages, err := projectConversation(state)
			if err != nil {
				return false, err
			}
			parentState, err := replayHistory(snapshot.Download.Batches, batch.Parent)
			if err != nil {
				return false, err
			}
			prior, err := projectConversation(parentState)
			if err != nil || len(messages) <= len(prior) {
				return false, errors.New("native publication did not append a visible message")
			}
			for index, previous := range prior {
				if messages[index] != previous {
					return false, errors.New("native publication changed an earlier message")
				}
			}
			id := sha256.Sum256(key)
			message := messages[len(prior)]
			segment, err := decodeVariant(expected)
			if err != nil {
				return false, err
			}
			role := "user"
			if segment.Tag == 1 {
				role = "assistant"
			}
			if message.ID == hex.EncodeToString(id[:16]) && message.Complete && message.Role == role && message.Text == text {
				return true, nil
			}
			return false, errors.New("native message text is not visible at the selected frontier")
		}
	}
	return false, nil
}

func samePublishedSegment(expected, current []byte) bool {
	a, aerr := decodeVariant(expected)
	b, berr := decodeVariant(current)
	if aerr != nil || berr != nil || a.Tag != b.Tag {
		return false
	}
	af, aerr := decodeFields(a.Payload, 0, 11)
	bf, berr := decodeFields(b.Payload, 0, 11)
	if aerr != nil || berr != nil || len(af) != len(bf) {
		return false
	}
	for index := range af {
		if a.Tag == 0 && (index == 5 || index == 6) {
			continue // Submission linkage may be filled by a later assistant.
		}
		if !bytes.Equal(af[index], bf[index]) {
			return false
		}
	}
	return true
}

var errPublicationUncertain = errors.New("native publication outcome is uncertain; retain and reconcile the same batch")

func publishNativeBatch(ctx context.Context, token string, actor uuid, thread threadID, expected string, batch historyDeltaBatch) error {
	before, err := fetchHistorySnapshot(ctx, defaultRemoteURL, token, actor, thread)
	if err != nil {
		return err
	}
	found, err := retainedBatchPresent(before, batch)
	if err != nil || found {
		return err
	}
	key, err := contextBridgeSession(before)
	if err != nil || key != expected {
		return errors.New("native thread changed before publication; retained batch was not sent")
	}
	request, err := historyUpdateRequest(thread, []historyDeltaBatch{batch}, []deltaID{batch.ID})
	if err != nil {
		return err
	}
	conn, _, err := connectRemote(ctx, defaultRemoteURL, token, actor, &thread.History)
	if err != nil {
		return err
	}
	s := newSession(ctx, conn)
	go func() {
		for range s.events {
		}
	}()
	response, sendErr := s.Call(ctx, request)
	s.Close()
	if sendErr == nil && response.Tag != 1 {
		return errors.New("native history publication was rejected")
	}
	after, err := fetchHistorySnapshot(ctx, defaultRemoteURL, token, actor, thread)
	if err != nil {
		return errPublicationUncertain
	}
	found, err = retainedBatchPresent(after, batch)
	if err != nil {
		return err
	}
	if !found {
		return errPublicationUncertain
	}
	return nil
}
