package main

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"strings"

	"github.com/vmihailenco/msgpack/v5"
)

const deltaProtocol = "delta-v49"

type uuid [16]byte

func newUUID() (uuid, error) {
	var id uuid
	if _, err := rand.Read(id[:]); err != nil {
		return id, err
	}
	id[6] = (id[6] & 0x0f) | 0x40
	id[8] = (id[8] & 0x3f) | 0x80
	return id, nil
}

func (id uuid) String() string {
	value := hex.EncodeToString(id[:])
	return value[:8] + "-" + value[8:12] + "-" + value[12:16] + "-" + value[16:20] + "-" + value[20:]
}

func parseUUID(value string) (uuid, error) {
	var id uuid
	if len(value) != 36 || value[8] != '-' || value[13] != '-' || value[18] != '-' || value[23] != '-' {
		return id, errors.New("UUID must use canonical hyphenated form")
	}
	raw, err := hex.DecodeString(strings.ReplaceAll(value, "-", ""))
	if err != nil || len(raw) != len(id) {
		return id, errors.New("invalid UUID")
	}
	copy(id[:], raw)
	return id, nil
}

func encodeWire(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := msgpack.NewEncoder(&buffer)
	encoder.UseCompactInts(true)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return buffer.Bytes(), nil
}

func encodeRequest(id uint32, tag uint64, payload any) ([]byte, error) {
	return encodeWire([]any{id, []any{tag, payload}})
}

func encodeMachineRunner(id uint32, name *string) ([]byte, error) {
	option := []any{}
	if name != nil {
		option = append(option, *name)
	}
	return encodeRequest(id, 51, []any{option})
}

func encodeHeartbeat(id uint32) ([]byte, error) {
	return encodeRequest(id, 33, []any{})
}

func encodeTurnAcknowledgment(id uint32, request uuid) ([]byte, error) {
	return encodeRequest(id, 53, []any{request[:], []any{}})
}

type wireVariant struct {
	Tag     uint64
	Payload msgpack.RawMessage
}

func decodeVariant(data []byte) (wireVariant, error) {
	var result wireVariant
	decoder := msgpack.NewDecoder(bytes.NewReader(data))
	code, err := decoder.PeekCode()
	if err != nil {
		return result, errors.New("empty protocol message")
	}
	if code <= 0x7f || code == 0xcc || code == 0xcd || code == 0xce || code == 0xcf {
		result.Tag, err = decoder.DecodeUint64()
	} else {
		var length int
		length, err = decoder.DecodeArrayLen()
		if err == nil && length != 2 {
			err = errors.New("protocol variant must have two elements")
		}
		if err == nil {
			result.Tag, err = decoder.DecodeUint64()
		}
		if err == nil {
			result.Payload, err = decoder.DecodeRaw()
		}
	}
	if err != nil {
		return wireVariant{}, errors.New("invalid protocol message")
	}
	if _, err := decoder.PeekCode(); !errors.Is(err, io.EOF) {
		return wireVariant{}, errors.New("trailing protocol data")
	}
	return result, nil
}

func decodeFields(data []byte, minimum, maximum int) ([]msgpack.RawMessage, error) {
	decoder := msgpack.NewDecoder(bytes.NewReader(data))
	length, err := decoder.DecodeArrayLen()
	if err != nil || length < minimum || length > 1024 {
		return nil, errors.New("invalid protocol field count")
	}
	fields := make([]msgpack.RawMessage, min(length, maximum))
	for i := 0; i < length; i++ {
		if i < len(fields) {
			fields[i], err = decoder.DecodeRaw()
		} else {
			err = decoder.Skip()
		}
		if err != nil {
			return nil, errors.New("invalid protocol field")
		}
	}
	if _, err := decoder.PeekCode(); !errors.Is(err, io.EOF) {
		return nil, errors.New("trailing protocol fields")
	}
	return fields, nil
}

func decodeUUID(data []byte) (uuid, error) {
	var id uuid
	var raw []byte
	if err := msgpack.Unmarshal(data, &raw); err != nil || len(raw) != len(id) {
		return id, errors.New("invalid protocol UUID")
	}
	copy(id[:], raw)
	return id, nil
}

type dispatchedTurn struct {
	Machine uuid
	Request uuid
	History uuid
	Start   msgpack.RawMessage
}

func decodeTurn(message wireVariant) (dispatchedTurn, error) {
	var turn dispatchedTurn
	if message.Tag != 18 {
		return turn, errors.New("message is not a machine-turn dispatch")
	}
	fields, err := decodeFields(message.Payload, 4, 4)
	if err != nil {
		return turn, err
	}
	targets := []*uuid{&turn.Machine, &turn.Request, &turn.History}
	for i, target := range targets {
		*target, err = decodeUUID(fields[i])
		if err != nil {
			return turn, fmt.Errorf("invalid turn identity field %d", i)
		}
	}
	if _, err := decodeFields(fields[3], 7, 7); err != nil {
		return turn, errors.New("invalid turn-start fields")
	}
	turn.Start = fields[3]
	return turn, nil
}
