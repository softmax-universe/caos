package main

import (
	"bytes"
	"errors"
)

type scratchAnchor struct {
	Kind      uint8
	Buffer    objectID
	Insertion deltaID
	Base      bool
	Offset    uint64
	RightBias bool
}

func decodeBool(data []byte) (bool, error) {
	switch {
	case bytes.Equal(data, []byte{0xc2}):
		return false, nil
	case bytes.Equal(data, []byte{0xc3}):
		return true, nil
	default:
		return false, errors.New("invalid protocol boolean")
	}
}

func decodeScratchAnchor(data []byte) (scratchAnchor, error) {
	var anchor scratchAnchor
	value, err := decodeVariant(data)
	if err != nil || value.Tag > 2 {
		return anchor, errors.New("invalid buffer anchor")
	}
	anchor.Kind = uint8(value.Tag)
	count := 1
	if value.Tag == 2 {
		count = 3
	}
	fields, err := decodeFields(value.Payload, count, count)
	if err != nil {
		return anchor, err
	}
	buffer, err := decodeVariant(fields[0])
	if err != nil || buffer.Tag != 0 {
		return anchor, errors.New("anchor does not identify a scratch buffer")
	}
	anchor.Buffer, err = decodeObjectID(buffer.Payload)
	if err != nil || value.Tag != 2 {
		return anchor, err
	}
	fragment, err := decodeFields(fields[1], 2, 2)
	if err != nil {
		return anchor, err
	}
	insertion, err := decodeFields(fragment[0], 2, 2)
	if err != nil {
		return anchor, err
	}
	if anchor.Insertion, err = decodeDeltaID(insertion[0]); err != nil {
		return anchor, err
	}
	if anchor.Base, err = decodeBool(insertion[1]); err != nil {
		return anchor, err
	}
	if anchor.Offset, err = decodeUint64(fragment[1]); err != nil {
		return anchor, err
	}
	bias, err := decodeUint64(fields[2])
	if err != nil || bias > 1 {
		return anchor, errors.New("invalid buffer anchor bias")
	}
	anchor.RightBias = bias == 1
	return anchor, nil
}

type sealedRange struct {
	Start, End scratchAnchor
}
