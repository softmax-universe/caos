package main

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"math"
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

type clientRequest struct {
	tag     uint64
	payload any
}

func (request clientRequest) encode(id uint32) ([]byte, error) {
	if request.tag > math.MaxUint8 {
		return nil, errors.New("protocol request tag exceeds one byte")
	}
	return encodeWire([]any{id, []any{request.tag, request.payload}})
}

func machineRunnerRequest(name *string) clientRequest {
	option := []any{}
	if name != nil {
		option = append(option, *name)
	}
	return clientRequest{51, []any{option}}
}

func heartbeatRequest() clientRequest {
	return clientRequest{33, []any{}}
}

func turnAcknowledgmentRequest(request uuid) clientRequest {
	return clientRequest{53, []any{request[:], []any{}}}
}

type wireVariant struct {
	Tag     uint64
	Payload msgpack.RawMessage
}

func decodeUnsigned(decoder *msgpack.Decoder, maximum uint64) (uint64, error) {
	code, err := decoder.PeekCode()
	if err != nil {
		return 0, err
	}
	var value uint64
	if code >= 0xd0 && code <= 0xd3 {
		var signed int64
		signed, err = decoder.DecodeInt64()
		if signed < 0 {
			return 0, errors.New("negative protocol integer")
		}
		value = uint64(signed)
	} else if code <= 0x7f || (code >= 0xcc && code <= 0xcf) {
		value, err = decoder.DecodeUint64()
	} else {
		return 0, errors.New("expected an unsigned protocol integer")
	}
	if err != nil || value > maximum {
		return 0, errors.New("protocol integer out of range")
	}
	return value, nil
}

func decodeVariant(data []byte) (wireVariant, error) {
	var result wireVariant
	decoder := msgpack.NewDecoder(bytes.NewReader(data))
	code, err := decoder.PeekCode()
	if err != nil {
		return result, errors.New("empty protocol message")
	}
	if code <= 0x7f || (code >= 0xcc && code <= 0xd3) {
		result.Tag, err = decodeUnsigned(decoder, math.MaxUint8)
	} else {
		var length int
		length, err = decoder.DecodeArrayLen()
		if err == nil && length != 2 {
			err = errors.New("protocol variant must have two elements")
		}
		if err == nil {
			result.Tag, err = decodeUnsigned(decoder, math.MaxUint8)
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

func decodeResponse(message wireVariant) (uint32, wireVariant, error) {
	if message.Tag != 5 {
		return 0, wireVariant{}, errors.New("message is not a protocol response")
	}
	fields, err := decodeFields(message.Payload, 2, 2)
	if err != nil {
		return 0, wireVariant{}, err
	}
	id, err := decodeUnsigned(msgpack.NewDecoder(bytes.NewReader(fields[0])), math.MaxUint32)
	if err != nil {
		return 0, wireVariant{}, errors.New("invalid response request id")
	}
	body, err := decodeVariant(fields[1])
	return uint32(id), body, err
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
