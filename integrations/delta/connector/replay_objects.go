package main

import (
	"bytes"
	"errors"
)

// Field references are opaque binary IDs. Versioned struct field paths are
// one source of them, not a restriction on the IDs the history can contain.
type objectID struct {
	Kind  uint8
	Delta deltaID
	Field string
}

func fieldObject(path ...byte) objectID {
	return objectID{Kind: 1, Field: string(path)}
}

func decodeObjectID(data []byte) (objectID, error) {
	var id objectID
	value, err := decodeVariant(data)
	if err != nil {
		return id, err
	}
	id.Kind = uint8(value.Tag)
	switch value.Tag {
	case 0:
		id.Delta, err = decodeDeltaID(value.Payload)
	case 1:
		var field []byte
		field, err = decodeBytes(value.Payload)
		id.Field = string(field)
	default:
		err = errors.New("unknown history object ID")
	}
	return id, err
}

func compareDeltaID(a, b deltaID) int {
	if a.Timestamp < b.Timestamp {
		return -1
	}
	if a.Timestamp > b.Timestamp {
		return 1
	}
	if a.Actor.Tag < b.Actor.Tag {
		return -1
	}
	if a.Actor.Tag > b.Actor.Tag {
		return 1
	}
	return bytes.Compare(a.Actor.ID[:], b.Actor.ID[:])
}

type historyValue struct {
	Write   deltaID
	Bytes   []byte
	Present bool
}

func assignHistoryValue(values map[objectID]historyValue, key objectID, write deltaID, value []byte) {
	if previous, ok := values[key]; !ok || compareDeltaID(write, previous.Write) > 0 {
		values[key] = historyValue{Write: write, Bytes: bytes.Clone(value), Present: true}
	}
}
