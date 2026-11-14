package main

import "errors"

var errAnchorNotVisible = errors.New("anchor addresses deleted text")

func (state *replayState) resolveAnchor(anchor scratchAnchor) (int, error) {
	return state.resolveScratchAnchor(anchor, false)
}

func (state *replayState) resolveVisibleAnchor(anchor scratchAnchor) (int, error) {
	return state.resolveScratchAnchor(anchor, true)
}

func (state *replayState) resolveScratchAnchor(anchor scratchAnchor, requireVisible bool) (int, error) {
	buffer := state.Buffers[anchor.Buffer]
	if buffer == nil {
		if anchor.Buffer.Kind != 1 || anchor.Kind == 2 {
			return 0, errors.New("anchor buffer or insertion is missing")
		}
		return 0, nil // An untouched field scratch buffer is implicitly empty.
	}
	if anchor.Kind == 0 {
		return 0, nil
	}
	if anchor.Kind == 2 && anchor.Base {
		return 0, errors.New("scratch anchor refers to an unavailable base insertion")
	}
	offset, resolved, deleted := 0, anchor.Kind == 1, false
	buffer.visit(state.basis, func(atom *textAtom, present bool) bool {
		if anchor.Kind == 2 && atom.Write == anchor.Insertion &&
			anchor.Offset >= uint64(atom.Offset) &&
			anchor.Offset < uint64(atom.Offset+len(atom.Text)) {
			if present {
				offset += int(anchor.Offset) - atom.Offset
				// Native Left anchors follow the addressed byte, while Right
				// anchors precede it. A deleted byte contributes no width.
				if !anchor.RightBias {
					offset++
				}
			}
			deleted = !present
			resolved = true
			return false
		}
		if present {
			offset += len(atom.Text)
		}
		return true
	})
	if !resolved {
		return 0, errors.New("anchor insertion or byte offset is missing")
	}
	if requireVisible && deleted {
		return 0, errAnchorNotVisible
	}
	return offset, nil
}
