package main

import (
	"bytes"
	"errors"
	"math"

	"github.com/vmihailenco/msgpack/v5"
)

type replaySeal struct {
	Buffer   objectID
	Write    deltaID
	Position historyPosition
	Basis    historyBasis
	Ranges   []sealedRange
}

func (state *replayState) buffer(id objectID) (*replayText, error) {
	if buffer := state.Buffers[id]; buffer != nil {
		return buffer, nil
	}
	if id.Kind != 1 {
		return nil, errors.New("scratch buffer has no creation delta")
	}
	buffer := &replayText{}
	state.Buffers[id] = buffer
	return buffer, nil
}

func (state *replayState) historyMap(id objectID) (*replayMap, error) {
	if values := state.Maps[id]; values != nil {
		return values, nil
	}
	if id.Kind != 1 {
		return nil, errors.New("map has no creation delta")
	}
	values := &replayMap{}
	state.Maps[id] = values
	return values, nil
}

func (state *replayState) apply(id deltaID, position historyPosition, basis historyBasis, payload wireVariant) error {
	switch payload.Tag {
	case 1: // CreateScratchBuffer.
		fields, err := decodeFields(payload.Payload, 1, 1)
		if err != nil {
			return err
		}
		text, err := decodeString(fields[0])
		if err != nil {
			return err
		}
		buffer := &replayText{}
		edit := textEdit{Text: text, Splices: []textSplice{{Inserted: len(text)}}}
		if err := buffer.apply(id, position, basis, edit); err != nil {
			return err
		}
		state.Buffers[objectID{Delta: id}] = buffer
	case 2: // ScratchBufferEdit.
		fields, err := decodeFields(payload.Payload, 2, 2)
		if err != nil {
			return err
		}
		target, err := decodeObjectID(fields[0])
		if err != nil {
			return err
		}
		edit, err := decodeTextEdit(fields[1])
		if err != nil {
			return err
		}
		buffer, err := state.buffer(target)
		if err != nil {
			return err
		}
		return buffer.apply(id, position, basis, edit)
	case 3: // CreateRegister.
		fields, err := decodeFields(payload.Payload, 1, 1)
		if err != nil {
			return err
		}
		value, err := decodeBytes(fields[0])
		if err != nil {
			return err
		}
		assignHistoryValue(state.Registers, objectID{Delta: id}, id, value)
	case 4: // RegisterSet.
		fields, err := decodeFields(payload.Payload, 2, 2)
		if err != nil {
			return err
		}
		target, err := decodeObjectID(fields[0])
		if err != nil {
			return err
		}
		value, err := decodeBytes(fields[1])
		if err != nil {
			return err
		}
		if _, exists := state.Registers[target]; !exists && target.Kind != 1 {
			return errors.New("register has no creation delta")
		}
		assignHistoryValue(state.Registers, target, id, value)
	case 5: // CreateMap.
		if _, err := decodeFields(payload.Payload, 0, 0); err != nil {
			return err
		}
		state.Maps[objectID{Delta: id}] = &replayMap{}
	case 6, 7: // MapInsert, MapRemove.
		count := 3
		if payload.Tag == 7 {
			count = 2
		}
		fields, err := decodeFields(payload.Payload, count, count)
		if err != nil {
			return err
		}
		target, err := decodeObjectID(fields[0])
		if err != nil {
			return err
		}
		key, err := decodeBytes(fields[1])
		if err != nil {
			return err
		}
		var value []byte
		if payload.Tag == 6 {
			value, err = decodeBytes(fields[2])
			if err != nil {
				return err
			}
		}
		values, err := state.historyMap(target)
		if err != nil {
			return err
		}
		values.assign(string(key), id, position, basis, value, payload.Tag == 6)
	case 12: // SetSerializedVersion.
		fields, err := decodeFields(payload.Payload, 1, 1)
		if err != nil {
			return err
		}
		version, err := decodeUint64(fields[0])
		if err != nil || version > math.MaxUint32 {
			return errors.New("invalid serialized history version")
		}
		if !state.Version.Present || compareDeltaID(id, state.Version.Write) > 0 {
			state.Version = historyValue{Write: id, Bytes: bytes.Clone(fields[0]), Present: true}
		}
	case 18: // BufferSeal: preserve protection metadata without changing text.
		return state.applySeal(id, position, basis, payload.Payload)
	default:
		return errors.New("history delta replay is not implemented for this variant")
	}
	return nil
}

func (state *replayState) applySeal(id deltaID, position historyPosition, basis historyBasis, data msgpack.RawMessage) error {
	fields, err := decodeFields(data, 2, 2)
	if err != nil {
		return err
	}
	buffer, err := decodeObjectID(fields[0])
	if err != nil {
		return err
	}
	ranges, err := decodeSequence(fields[1], maxHistoryItems)
	if err != nil {
		return err
	}
	seal := replaySeal{Buffer: buffer, Write: id, Position: position, Basis: basis}
	for _, raw := range ranges {
		ends, err := decodeFields(raw, 2, 2)
		if err != nil {
			return err
		}
		start, err := decodeScratchAnchor(ends[0])
		if err != nil {
			return err
		}
		end, err := decodeScratchAnchor(ends[1])
		if err != nil {
			return err
		}
		if start.Buffer != buffer || end.Buffer != buffer {
			return errors.New("seal anchors identify a different buffer")
		}
		seal.Ranges = append(seal.Ranges, sealedRange{Start: start, End: end})
	}
	if _, err := state.buffer(buffer); err != nil {
		return err
	}
	state.Seals = append(state.Seals, seal)
	return nil
}
