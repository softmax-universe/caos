package main

import (
	"errors"
	"fmt"
	"reflect"
	"sort"
)

type replayBatch struct {
	Batch     historyDeltaBatch
	Index     int
	Ancestors *historyAncestry
}

func planHistory(batches []historyDeltaBatch, heads []deltaID) ([]replayBatch, *historyAncestry, error) {
	if len(batches) > maxHistoryItems {
		return nil, nil, errors.New("too many history batches")
	}
	byID := make(map[deltaID]historyDeltaBatch, len(batches))
	for _, batch := range batches {
		if previous, exists := byID[batch.ID]; exists && !reflect.DeepEqual(previous, batch) {
			return nil, nil, errors.New("conflicting copies of one history batch")
		}
		byID[batch.ID] = batch
	}
	// A replication pack can cover other timelines. Select the requested
	// frontier's closure rather than replaying every returned record.
	selected := make(map[deltaID]bool)
	pending := append([]deltaID(nil), heads...)
	for len(pending) > 0 {
		id := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if selected[id] {
			continue
		}
		batch, exists := byID[id]
		if !exists {
			return nil, nil, errors.New("history is missing a requested head or causal parent")
		}
		selected[id] = true
		pending = append(pending, batch.Parent...)
	}
	var ids []deltaID
	for id := range selected {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool { return compareDeltaID(ids[i], ids[j]) < 0 })
	remaining := make(map[deltaID]int)
	children := make(map[deltaID][]deltaID)
	var ready []deltaID
	for _, id := range ids {
		parents := make(map[deltaID]bool)
		for _, parent := range byID[id].Parent {
			parents[parent] = true
		}
		remaining[id] = len(parents)
		if len(parents) == 0 {
			ready = append(ready, id)
		}
		for parent := range parents {
			children[parent] = append(children[parent], id)
		}
	}
	closures := make(map[deltaID]*historyAncestry)
	var plan []replayBatch
	var all *historyAncestry
	for cursor := 0; cursor < len(ready); cursor++ {
		id := ready[cursor]
		batch := byID[id]
		var ancestors *historyAncestry
		for _, parent := range batch.Parent {
			ancestors = unionAncestors(ancestors, closures[parent])
		}
		index := len(plan)
		plan = append(plan, replayBatch{Batch: batch, Index: index, Ancestors: ancestors})
		closures[id] = addAncestor(ancestors, index, ancestryDepth)
		all = addAncestor(all, index, ancestryDepth)
		for _, child := range children[id] {
			remaining[child]--
			if remaining[child] == 0 {
				ready = append(ready, child)
			}
		}
	}
	if len(plan) != len(selected) {
		return nil, nil, errors.New("history contains a causal cycle")
	}
	return plan, all, nil
}

func payloadID(batch historyDeltaBatch, index int) (deltaID, error) {
	if index < 0 || index >= len(batch.Payloads) || batch.ID.Timestamp < uint64(len(batch.Payloads)-1) {
		return deltaID{}, errors.New("invalid history payload clock span")
	}
	id := batch.ID
	id.Timestamp -= uint64(len(batch.Payloads) - 1 - index)
	return id, nil
}

type replayState struct {
	Batches   int
	Registers map[objectID]historyValue
	Maps      map[objectID]*replayMap
	Buffers   map[objectID]*replayText
	Seals     []replaySeal
	Version   historyValue
	basis     historyBasis
}

// Replay reconstructs accepted records. It is not authorization for new edits:
// a writer must additionally enforce seals, generation ownership, and epochs.
func replayHistory(batches []historyDeltaBatch, heads []deltaID) (*replayState, error) {
	plan, all, err := planHistory(batches, heads)
	if err != nil {
		return nil, err
	}
	state := &replayState{
		Batches:   len(plan),
		Registers: make(map[objectID]historyValue),
		Maps:      make(map[objectID]*replayMap),
		Buffers:   make(map[objectID]*replayText),
		basis:     historyBasis{Ancestors: all, Current: historyPosition{Batch: -1}},
	}
	seen := make(map[deltaID]bool)
	for _, item := range plan {
		for index, payload := range item.Batch.Payloads {
			id, err := payloadID(item.Batch, index)
			if err != nil {
				return nil, err
			}
			if seen[id] {
				return nil, errors.New("history batches have overlapping payload identities")
			}
			seen[id] = true
			position := historyPosition{Batch: item.Index, Payload: index}
			basis := historyBasis{Ancestors: item.Ancestors, Current: position}
			if err := state.apply(id, position, basis, payload); err != nil {
				return nil, fmt.Errorf("history delta %d: %w", payload.Tag, err)
			}
		}
	}
	return state, nil
}

func replaySnapshot(snapshot historySnapshot) (*replayState, error) {
	if len(snapshot.Download.ReplicationPacks) != 0 {
		return nil, errors.New("history replication-pack download is not implemented")
	}
	if int(snapshot.Download.CoveredBatchCount) != len(snapshot.Download.Batches) {
		return nil, errors.New("history download does not cover its declared batches")
	}
	var heads []deltaID
	found := false
	for _, raw := range snapshot.Timelines {
		fields, err := decodeFields(raw, 2, 2)
		if err != nil {
			return nil, err
		}
		timeline, err := decodeDeltaID(fields[0])
		if err != nil {
			return nil, err
		}
		if timeline != snapshot.Timeline {
			continue
		}
		if found {
			return nil, errors.New("history index contains duplicate timeline metadata")
		}
		heads, err = decodeHistoryVersion(fields[1])
		if err != nil {
			return nil, err
		}
		found = true
	}
	if !found {
		return nil, errors.New("history index is missing the requested timeline")
	}
	return replayHistory(snapshot.Download.Batches, heads)
}
