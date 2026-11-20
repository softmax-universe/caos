package main

import "bytes"

type mapVersion struct {
	Value     historyValue
	Birth     historyPosition
	RemovedBy []historyPosition
}

// Delta maps retain concurrent values. A write or removal replaces only values
// observed in its causal parent; a concurrent, unseen insertion must survive.
// Reading chooses the greatest delta ID among the remaining visible values.
type replayMap struct {
	entries map[string][]*mapVersion
}

func (values *replayMap) assign(key string, id deltaID, position historyPosition, basis historyBasis, data []byte, present bool) {
	if values.entries == nil {
		values.entries = make(map[string][]*mapVersion)
	}
	for _, candidate := range values.entries[key] {
		if basis.contains(candidate.Birth) && !removedAt(candidate.RemovedBy, basis) {
			candidate.RemovedBy = append(candidate.RemovedBy, position)
		}
	}
	if present {
		values.entries[key] = append(values.entries[key], &mapVersion{
			Value: historyValue{Write: id, Bytes: bytes.Clone(data), Present: true}, Birth: position,
		})
	}
}

func (values *replayMap) get(key string, basis historyBasis) historyValue {
	var winner historyValue
	for _, candidate := range values.entries[key] {
		if !basis.contains(candidate.Birth) || removedAt(candidate.RemovedBy, basis) {
			continue
		}
		if !winner.Present || compareDeltaID(candidate.Value.Write, winner.Write) > 0 {
			winner = candidate.Value
		}
	}
	return winner
}
