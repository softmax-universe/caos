package main

import (
	"encoding/base64"
	"errors"
	"fmt"
	"math"

	"github.com/vmihailenco/msgpack/v5"
)

const maxHistoryItems = 1 << 16

type threadID struct {
	History  uuid
	Timeline deltaID
}

func parseThreadID(value string) (threadID, error) {
	var id threadID
	if len(value) == 0 || len(value) > 512 {
		return id, errors.New("invalid Delta thread ID")
	}
	raw, err := base64.RawURLEncoding.Strict().DecodeString(value)
	if err != nil {
		return id, errors.New("invalid Delta thread ID encoding")
	}
	fields, err := decodeFields(raw, 2, 2)
	if err != nil {
		return id, err
	}
	if id.History, err = decodeUUID(fields[0]); err != nil {
		return id, err
	}
	id.Timeline, err = decodeDeltaID(fields[1])
	return id, err
}

func (id deltaID) wireValue() ([]any, error) {
	size := 20
	switch id.Actor.Tag {
	case 0:
	case 1:
		size = 16
		for _, value := range id.Actor.ID[16:] {
			if value != 0 {
				return nil, errors.New("noncanonical UUID history actor")
			}
		}
	default:
		return nil, errors.New("invalid history actor tag")
	}
	return []any{id.Timestamp, []any{id.Actor.Tag, id.Actor.ID[:size]}}, nil
}

func historySinceRequest(thread threadID, version []deltaID) (clientRequest, error) {
	timeline, err := thread.Timeline.wireValue()
	if err != nil {
		return clientRequest{}, err
	}
	parent := make([]any, len(version))
	for i, id := range version {
		value, err := id.wireValue()
		if err != nil {
			return clientRequest{}, err
		}
		parent[i] = value
	}
	return clientRequest{6, []any{thread.History[:], timeline, parent}}, nil
}

func historyDeltasRequest(history uuid, compressedIDs msgpack.RawMessage) (clientRequest, error) {
	fields, err := decodeFields(compressedIDs, 1, 1)
	if err != nil {
		return clientRequest{}, err
	}
	if len(fields[0]) == 0 || fields[0][0] < 0xc4 || fields[0][0] > 0xc6 {
		return clientRequest{}, errors.New("invalid compressed history IDs")
	}
	// CompressedDeltaBatchIds is a one-field struct containing binary data.
	// Forward the index's value unchanged, not as delta-batch payloads.
	return clientRequest{27, []any{history[:], compressedIDs}}, nil
}

type historyDeltaBatch struct {
	ID       deltaID
	Parent   []deltaID
	Payloads []wireVariant
}

func decodeHistoryBatch(data []byte) (historyDeltaBatch, error) {
	var batch historyDeltaBatch
	fields, err := decodeFields(data, 4, 4)
	if err != nil {
		return batch, err
	}
	if batch.ID, err = decodeDeltaID(fields[0]); err != nil {
		return batch, err
	}
	parent, err := decodeSequence(fields[2], maxHistoryItems)
	if err != nil {
		return batch, err
	}
	for _, raw := range parent {
		id, err := decodeDeltaID(raw)
		if err != nil {
			return batch, err
		}
		batch.Parent = append(batch.Parent, id)
	}
	payloads, err := decodeSequence(fields[3], maxHistoryItems)
	if err != nil {
		return batch, err
	}
	for _, raw := range payloads {
		value, err := decodeVariant(raw)
		if err != nil {
			return batch, err
		}
		batch.Payloads = append(batch.Payloads, value)
	}
	return batch, nil
}

type historyDownload struct {
	CoveredBatchCount uint32
	Batches           []historyDeltaBatch
	ReplicationPacks  []msgpack.RawMessage
}

func decodeHistoryDownload(data []byte) (historyDownload, error) {
	var download historyDownload
	value, err := decodeVariant(data)
	if err != nil {
		return download, err
	}
	var batches msgpack.RawMessage
	switch value.Tag {
	case 0: // Deltas.
		fields, err := decodeFields(value.Payload, 1, 1)
		if err != nil {
			return download, err
		}
		batches = fields[0]
	case 2: // DeltasAndReplicationPacks.
		fields, err := decodeFields(value.Payload, 3, 3)
		if err != nil {
			return download, err
		}
		count, err := decodeUint64(fields[0])
		if err != nil || count > math.MaxUint32 {
			return download, errors.New("invalid covered history batch count")
		}
		download.CoveredBatchCount = uint32(count)
		batches = fields[1]
		download.ReplicationPacks, err = decodeSequence(fields[2], maxHistoryItems)
		if err != nil {
			return download, err
		}
	default:
		// Includes Superseded, Error, and Unsupported. None is an empty or
		// complete history, and server-supplied diagnostic contents stay private.
		return download, fmt.Errorf("Delta history download was not accepted (variant %d)", value.Tag)
	}
	rawBatches, err := decodeSequence(batches, maxHistoryItems)
	if err != nil {
		return download, err
	}
	for _, raw := range rawBatches {
		batch, err := decodeHistoryBatch(raw)
		if err != nil {
			return download, err
		}
		download.Batches = append(download.Batches, batch)
	}
	if value.Tag == 0 {
		download.CoveredBatchCount = uint32(len(download.Batches))
	}
	return download, nil
}
