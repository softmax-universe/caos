package main

import (
	"bytes"
	"errors"
	"fmt"
	"math"
	"unicode/utf8"

	"github.com/vmihailenco/msgpack/v5"
)

// This is the dispatch's start request, not evidence that this runner owns the
// turn. Admission still has to check the synchronized history and generation
// state before acknowledging the request or starting any execution.
type startTurn struct {
	Sender                        uuid
	Epoch                         uint64
	Organization                  *string
	GenerationStateWriteTimestamp *uint64
	AssistantTurn                 *string
	Timeline                      *deltaID
	RunnerStarted                 bool
}

// History and timeline IDs share the same wire representation. An actor is
// either a Git commit (tag 0, 20 bytes) or a UUID (tag 1, 16 bytes). UUID actors
// leave the final four ID bytes zero so decoded IDs are comparable map keys.
type historyActor struct {
	Tag uint8
	ID  [20]byte
}

type deltaID struct {
	Timestamp uint64
	Actor     historyActor
}

func decodeBinary(data []byte, size int) ([]byte, error) {
	raw, err := decodeBytes(data)
	if err != nil || len(raw) != size {
		return nil, errors.New("invalid protocol binary length")
	}
	return raw, nil
}

func decodeBytes(data []byte) ([]byte, error) {
	if len(data) == 0 || data[0] < 0xc4 || data[0] > 0xc6 {
		return nil, errors.New("expected protocol binary")
	}
	var raw []byte
	if err := msgpack.Unmarshal(data, &raw); err != nil {
		return nil, errors.New("invalid protocol binary length")
	}
	return raw, nil
}

func decodeString(data []byte) (string, error) {
	if len(data) == 0 || !((data[0] >= 0xa0 && data[0] <= 0xbf) ||
		(data[0] >= 0xd9 && data[0] <= 0xdb)) {
		return "", errors.New("expected protocol string")
	}
	var value string
	err := msgpack.Unmarshal(data, &value)
	if err == nil && !utf8.ValidString(value) {
		err = errors.New("invalid protocol UTF-8")
	}
	return value, err
}

func decodeUint64(data []byte) (uint64, error) {
	return decodeUnsigned(msgpack.NewDecoder(bytes.NewReader(data)), math.MaxUint64)
}

func decodeOption[T any](data []byte, decode func([]byte) (T, error)) (*T, error) {
	// The canonical encoding is []/[value]. The reference decoder also accepts
	// nil for a defaulted field, and an omitted trailing field has that default.
	if len(data) == 0 || bytes.Equal(data, []byte{0xc0}) {
		return nil, nil
	}
	fields, err := decodeFields(data, 0, 2)
	if err != nil || len(fields) > 1 {
		return nil, errors.New("invalid protocol option")
	}
	if len(fields) == 0 {
		return nil, nil
	}
	value, err := decode(fields[0])
	if err != nil {
		return nil, err
	}
	return &value, nil
}

func decodeDeltaID(data []byte) (deltaID, error) {
	var id deltaID
	fields, err := decodeFields(data, 2, 2)
	if err != nil {
		return id, err
	}
	id.Timestamp, err = decodeUint64(fields[0])
	if err != nil {
		return id, err
	}
	actor, err := decodeVariant(fields[1])
	if err != nil || actor.Tag > 1 {
		return id, errors.New("invalid history actor")
	}
	size := 20
	if actor.Tag == 1 {
		size = 16
	}
	raw, err := decodeBinary(actor.Payload, size)
	if err != nil {
		return id, err
	}
	id.Actor.Tag = uint8(actor.Tag)
	copy(id.Actor.ID[:], raw)
	return id, nil
}

func decodeStartTurn(data []byte) (startTurn, error) {
	var turn startTurn
	fields, err := decodeFields(data, 2, 7)
	if err != nil {
		return turn, err
	}
	turn.Sender, err = decodeUUID(fields[0])
	if err != nil {
		return turn, err
	}
	turn.Epoch, err = decodeUint64(fields[1])
	if err != nil {
		return turn, err
	}
	// Only sender and epoch are required. The remaining fields were added
	// with defaults, which must apply to both omitted fields and explicit nil.
	fields = append(fields, make([]msgpack.RawMessage, 7-len(fields))...)
	if turn.Organization, err = decodeOption(fields[2], decodeString); err != nil {
		return turn, fmt.Errorf("organization: %w", err)
	}
	if turn.GenerationStateWriteTimestamp, err = decodeOption(fields[3], decodeUint64); err != nil {
		return turn, fmt.Errorf("generation-state timestamp: %w", err)
	}
	if turn.AssistantTurn, err = decodeOption(fields[4], decodeString); err != nil {
		return turn, fmt.Errorf("assistant turn: %w", err)
	}
	if turn.Timeline, err = decodeOption(fields[5], decodeDeltaID); err != nil {
		return turn, fmt.Errorf("timeline: %w", err)
	}
	switch {
	case len(fields[6]) == 0, bytes.Equal(fields[6], []byte{0xc0}),
		bytes.Equal(fields[6], []byte{0xc2}):
	case bytes.Equal(fields[6], []byte{0xc3}):
		turn.RunnerStarted = true
	default:
		return turn, errors.New("invalid runner-started flag")
	}
	return turn, nil
}
