package main

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"sort"
	"strings"
	"unicode/utf8"
)

type agentMessage struct {
	ID       string
	Role     string
	Text     string
	Complete bool
}

type conversationSegment struct {
	Key    string
	Offset int
	Value  wireVariant
}

// Project native records into separate, stable messages for the existing CAOS
// session driver. Never forward the whole segment value: user metadata can
// contain an inference voucher, which is not conversation content.
func projectConversation(state *replayState) ([]agentMessage, error) {
	if value := state.Registers[fieldObject(55)]; value.Present {
		parent, err := decodeOption(value.Bytes, func([]byte) (bool, error) { return true, nil })
		if err != nil || parent != nil {
			return nil, errors.New("parent conversation-context import is not implemented")
		}
	}
	for _, field := range []byte{9, 20, 60, 65, 79} {
		if values := state.Maps[fieldObject(field)]; values != nil {
			for key := range values.entries {
				if values.get(key, state.basis).Present {
					return nil, errors.New("conversation projection for attachments or assistant round boundaries is not implemented")
				}
			}
		}
	}
	text := ""
	if buffer := state.Buffers[fieldObject(0)]; buffer != nil {
		text = buffer.stringAt(state.basis)
	}
	var segments []conversationSegment
	if values := state.Maps[fieldObject(1)]; values != nil {
		for key := range values.entries {
			value := values.get(key, state.basis)
			if !value.Present {
				continue
			}
			anchor, err := decodeScratchAnchor([]byte(key))
			if err != nil || anchor.Buffer != fieldObject(0) {
				return nil, errors.New("message anchor does not identify the thread buffer")
			}
			offset, err := state.resolveVisibleAnchor(anchor)
			if errors.Is(err, errAnchorNotVisible) {
				continue
			}
			if err != nil {
				return nil, err
			}
			segment, err := decodeVariant(value.Bytes)
			if err != nil {
				return nil, err
			}
			if segment.Tag != 0 && segment.Tag != 1 {
				return nil, errors.New("structured conversation-segment projection is not implemented")
			}
			segments = append(segments, conversationSegment{Key: key, Offset: offset, Value: segment})
		}
	}
	sort.Slice(segments, func(i, j int) bool { return segments[i].Offset < segments[j].Offset })
	var messages []agentMessage
	for index, segment := range segments {
		start, end := segment.Offset, len(text)
		if index+1 < len(segments) {
			end = segments[index+1].Offset
			if end == start {
				return nil, errors.New("coincident message-boundary ordering is not implemented")
			}
		}
		role, complete := "assistant", false
		enclosed := false
		if segment.Value.Tag == 0 {
			role, complete = "user", true
			fields, err := decodeFields(segment.Value.Payload, 0, 8)
			if err != nil {
				return nil, err
			}
			if len(fields) > 4 {
				enclosure, err := decodeOption(fields[4], func(raw []byte) (int, error) {
					ends, err := decodeFields(raw, 2, 2)
					if err != nil {
						return 0, err
					}
					first, err := decodeScratchAnchor(ends[0])
					if err != nil || first.Buffer != fieldObject(0) {
						return 0, errors.New("invalid user enclosure start")
					}
					last, err := decodeScratchAnchor(ends[1])
					if err != nil || last.Buffer != fieldObject(0) {
						return 0, errors.New("invalid user enclosure end")
					}
					storageStart, err := state.resolveVisibleAnchor(first)
					if err != nil || storageStart > start {
						return 0, errors.New("user enclosure starts after its message")
					}
					return state.resolveVisibleAnchor(last)
				})
				if err != nil {
					return nil, err
				}
				if enclosure != nil {
					if *enclosure > end {
						return nil, errors.New("user enclosure overlaps the next message")
					}
					end, enclosed = *enclosure, true
				}
			}
		} else {
			fields, err := decodeFields(segment.Value.Payload, 0, 11)
			if err != nil {
				return nil, err
			}
			if len(fields) > 8 {
				completed, err := decodeOption(fields[8], decodeUint64)
				if err != nil {
					return nil, err
				}
				complete = completed != nil
			}
		}
		if start < 0 || end < start || end > len(text) {
			return nil, errors.New("invalid message content range")
		}
		// The native source-range rule removes the stored separator, not
		// arbitrary whitespace. Explicit user enclosures already name content.
		if !enclosed && strings.HasSuffix(text[start:end], "\n\n") {
			end -= 2
		}
		content := text[start:end]
		if !utf8.ValidString(content) {
			return nil, errors.New("message boundary splits a UTF-8 scalar")
		}
		identity := sha256.Sum256([]byte(segment.Key))
		messages = append(messages, agentMessage{
			ID: hex.EncodeToString(identity[:16]), Role: role, Text: content, Complete: complete,
		})
	}
	return messages, nil
}
