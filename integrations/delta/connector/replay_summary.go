package main

type replaySummary struct {
	Batches            int
	Registers          int
	Maps               int
	Buffers            int
	RecordedSealRanges int
	ThreadTextBytes    int
	GenerationEpoch    uint64
	GenerationStateTag *uint64
}

func summarizeReplay(state *replayState) (replaySummary, error) {
	summary := replaySummary{
		Batches: state.Batches, Registers: len(state.Registers),
		Maps: len(state.Maps), Buffers: len(state.Buffers),
	}
	if buffer := state.Buffers[fieldObject(0)]; buffer != nil {
		summary.ThreadTextBytes = len(buffer.stringAt(state.basis))
	}
	for _, seal := range state.Seals {
		summary.RecordedSealRanges += len(seal.Ranges)
	}
	if epoch, exists := state.Registers[fieldObject(32)]; exists {
		value, err := decodeUint64(epoch.Bytes)
		if err != nil {
			return summary, err
		}
		summary.GenerationEpoch = value
	}
	if generation, exists := state.Registers[fieldObject(23)]; exists {
		value, err := decodeVariant(generation.Bytes)
		if err != nil {
			return summary, err
		}
		// This is persisted state, not a liveness test: a completed assistant
		// turn can still be associated with a stored Streaming variant.
		summary.GenerationStateTag = &value.Tag
	}
	return summary, nil
}
