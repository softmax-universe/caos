package main

import (
	"errors"
	"math"

	"github.com/vmihailenco/msgpack/v5"
)

// Allocate a single causal batch after a synchronized frontier. The caller must
// serialize writers using this actor and retain the returned batch across an
// uncertain send; allocating again is not a retry.
func newHistoryBatch(snapshot historySnapshot, actor uuid, payloads []wireVariant) (historyDeltaBatch, error) {
	var batch historyDeltaBatch
	if len(payloads) == 0 || len(payloads) > maxHistoryItems {
		return batch, errors.New("invalid outgoing history payload count")
	}
	heads, err := snapshot.frontier()
	if err != nil {
		return batch, err
	}
	_, _, err = planHistory(snapshot.Download.Batches, heads)
	if err != nil || len(snapshot.Download.ReplicationPacks) != 0 ||
		int(snapshot.Download.CoveredBatchCount) != len(snapshot.Download.Batches) {
		return batch, errors.New("cannot write against incomplete history")
	}
	var clock uint64
	for _, known := range snapshot.Download.Batches {
		if known.ID.Timestamp > clock {
			clock = known.ID.Timestamp
		}
	}
	if clock > math.MaxUint64-uint64(len(payloads)) {
		return batch, errors.New("history clock exhausted")
	}
	batch.ID.Timestamp = clock + uint64(len(payloads))
	batch.ID.Actor.Tag = 1
	copy(batch.ID.Actor.ID[:], actor[:])
	batch.Parent = append([]deltaID(nil), heads...)
	batch.Payloads = make([]wireVariant, len(payloads))
	for index, payload := range payloads {
		batch.Payloads[index] = wireVariant{Tag: payload.Tag, Payload: append([]byte(nil), payload.Payload...)}
	}
	return batch, nil
}

func historyVersionValue(version []deltaID) ([]any, error) {
	values := make([]any, len(version))
	for index, id := range version {
		value, err := id.wireValue()
		if err != nil {
			return nil, err
		}
		values[index] = value
	}
	return values, nil
}

func historyBatchValue(batch historyDeltaBatch) ([]any, error) {
	id, err := batch.ID.wireValue()
	if err != nil {
		return nil, err
	}
	parent, err := historyVersionValue(batch.Parent)
	if err != nil {
		return nil, err
	}
	payloads := make([]any, len(batch.Payloads))
	for index, payload := range batch.Payloads {
		if payload.Tag > 255 {
			return nil, errors.New("invalid history delta tag")
		}
		if len(payload.Payload) == 0 {
			payloads[index] = payload.Tag
		} else {
			// DecodeRaw has already delimited this value. New writers must
			// construct and validate their typed operation before reaching here.
			payloads[index] = []any{payload.Tag, msgpack.RawMessage(payload.Payload)}
		}
	}
	return []any{id, nil, parent, payloads}, nil
}

// This forms a protocol write, not permission to make it. The turn controller
// must separately enforce the binding, epoch, ownership and protected ranges.
func historyUpdateRequest(thread threadID, batches []historyDeltaBatch, frontier []deltaID) (clientRequest, error) {
	values := make([]any, len(batches))
	for index, batch := range batches {
		value, err := historyBatchValue(batch)
		if err != nil {
			return clientRequest{}, err
		}
		values[index] = value
	}
	timeline, err := thread.Timeline.wireValue()
	if err != nil {
		return clientRequest{}, err
	}
	version, err := historyVersionValue(frontier)
	if err != nil {
		return clientRequest{}, err
	}
	updates := []any{}
	if frontier != nil {
		updates = append(updates, []any{timeline, version})
	}
	return clientRequest{1, []any{thread.History[:], values, updates, []any{}, []any{}}}, nil
}
