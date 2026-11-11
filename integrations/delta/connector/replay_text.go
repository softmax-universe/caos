package main

import (
	"errors"
	"sort"
	"strings"
	"unicode/utf8"
)

type textAtom struct {
	Write     deltaID
	Offset    int
	Text      string
	Birth     historyPosition
	DeletedBy []historyPosition
	Children  []*textAtom
}

// Insertions retain their identity after deletion. An edit is interpreted in
// its causal parent's view, so a concurrent delete cannot erase unseen text.
type replayText struct {
	root textAtom
}

func (text *replayText) visible(basis historyBasis) []*textAtom {
	type cursor struct {
		node  *textAtom
		child int
	}
	stack := []cursor{{node: &text.root}}
	var visible []*textAtom
	for len(stack) > 0 {
		top := &stack[len(stack)-1]
		if top.child == len(top.node.Children) {
			stack = stack[:len(stack)-1]
			continue
		}
		node := top.node.Children[top.child]
		top.child++
		if !basis.contains(node.Birth) {
			continue // A visible descendant cannot have an unseen insertion parent.
		}
		if !removedAt(node.DeletedBy, basis) {
			visible = append(visible, node)
		}
		stack = append(stack, cursor{node: node})
	}
	return visible
}

func (text *replayText) stringAt(basis historyBasis) string {
	var result strings.Builder
	for _, atom := range text.visible(basis) {
		result.WriteString(atom.Text)
	}
	return result.String()
}

func insertTextChild(parent, child *textAtom) {
	index := sort.Search(len(parent.Children), func(i int) bool {
		other := parent.Children[i]
		if order := compareDeltaID(child.Write, other.Write); order != 0 {
			return order > 0 // Concurrent insertions sort by descending delta ID.
		}
		return child.Offset < other.Offset
	})
	parent.Children = append(parent.Children, nil)
	copy(parent.Children[index+1:], parent.Children[index:])
	parent.Children[index] = child
}

func (text *replayText) apply(write deltaID, position historyPosition, basis historyBasis, edit textEdit) error {
	view := text.visible(basis)
	boundaries := map[int]int{0: 0}
	length := 0
	for i, atom := range view {
		length += len(atom.Text)
		boundaries[length] = i + 1
	}
	type preparedSplice struct {
		parent       *textAtom
		deleted      []*textAtom
		inserted     string
		insertOffset int
	}
	var prepared []preparedSplice
	insertOffset, previousEnd := 0, 0
	for _, splice := range edit.Splices {
		if splice.Start < 0 || splice.Deleted < 0 || splice.Inserted < 0 ||
			splice.Start < previousEnd || splice.Start > length || splice.Deleted > length-splice.Start {
			return errors.New("text splice is outside or overlaps its causal parent")
		}
		start, startOK := boundaries[splice.Start]
		end, endOK := boundaries[splice.Start+splice.Deleted]
		if !startOK || !endOK {
			return errors.New("text splice splits a UTF-8 character")
		}
		if splice.Inserted > len(edit.Text)-insertOffset {
			return errors.New("text splice insertion exceeds its payload")
		}
		inserted := edit.Text[insertOffset : insertOffset+splice.Inserted]
		if !utf8.ValidString(inserted) {
			return errors.New("text splice insertion splits a UTF-8 character")
		}
		parent := &text.root
		if start > 0 {
			parent = view[start-1]
		}
		prepared = append(prepared, preparedSplice{
			parent: parent, deleted: view[start:end], inserted: inserted, insertOffset: insertOffset,
		})
		insertOffset += splice.Inserted
		previousEnd = splice.Start + splice.Deleted
	}
	if insertOffset != len(edit.Text) {
		return errors.New("unconsumed text splice insertion payload")
	}
	// Validate the whole edit before making any change.
	for _, splice := range prepared {
		for _, atom := range splice.deleted {
			atom.DeletedBy = append(atom.DeletedBy, position)
		}
		parent := splice.parent
		for offset, character := range splice.inserted {
			size := utf8.RuneLen(character)
			child := &textAtom{
				Write: write, Offset: splice.insertOffset + offset,
				Text: splice.inserted[offset : offset+size], Birth: position,
			}
			insertTextChild(parent, child)
			parent = child
		}
	}
	return nil
}
