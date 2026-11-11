package main

import (
	"errors"
	"math"
)

type textSplice struct {
	Start, Deleted, Inserted int
}

type textEdit struct {
	Text    string
	Splices []textSplice
}

func decodeTextEdit(data []byte) (textEdit, error) {
	var edit textEdit
	fields, err := decodeSequence(data, maxHistoryItems)
	if err != nil || len(fields) == 0 || (len(fields)-1)%3 != 0 {
		return edit, errors.New("invalid text edit shape")
	}
	edit.Text, err = decodeString(fields[0])
	if err != nil {
		return edit, err
	}
	inserted := 0
	for i := 1; i < len(fields); i += 3 {
		var values [3]int
		for j := range values {
			value, err := decodeUint64(fields[i+j])
			if err != nil || value > math.MaxInt {
				return edit, errors.New("invalid text edit offset")
			}
			values[j] = int(value)
		}
		splice := textSplice{Start: values[0], Deleted: values[1], Inserted: values[2]}
		if splice.Inserted > len(edit.Text)-inserted || splice.Deleted > math.MaxInt-splice.Start {
			return edit, errors.New("text edit length overflow")
		}
		inserted += splice.Inserted
		edit.Splices = append(edit.Splices, splice)
	}
	if inserted != len(edit.Text) {
		return edit, errors.New("text edit insertion lengths do not match its text")
	}
	return edit, nil
}
