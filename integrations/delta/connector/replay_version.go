package main

// A persistent bitmap records actual batch ancestry, not just maximum actor
// clocks: two forks can have gaps and incomparable writes by the same actor.
// The tree covers maxHistoryItems bits. Sharing unchanged nodes makes a linear
// history cost O(log n) new nodes per batch rather than copying every ancestor.
type historyAncestry struct {
	left, right *historyAncestry
	bits        uint64
}

const ancestryDepth = 10 // 2^10 leaves * 64 bits = maxHistoryItems.

func addAncestor(tree *historyAncestry, index, depth int) *historyAncestry {
	node := historyAncestry{}
	if tree != nil {
		node = *tree
	}
	if depth == 0 {
		node.bits |= uint64(1) << (index & 63)
	} else if index&(1<<(depth+5)) == 0 {
		node.left = addAncestor(node.left, index, depth-1)
	} else {
		node.right = addAncestor(node.right, index, depth-1)
	}
	return &node
}

func unionAncestors(a, b *historyAncestry) *historyAncestry {
	if a == b || b == nil {
		return a
	}
	if a == nil {
		return b
	}
	left, right := unionAncestors(a.left, b.left), unionAncestors(a.right, b.right)
	bits := a.bits | b.bits
	if left == a.left && right == a.right && bits == a.bits {
		return a
	}
	if left == b.left && right == b.right && bits == b.bits {
		return b
	}
	return &historyAncestry{left: left, right: right, bits: bits}
}

func hasAncestor(tree *historyAncestry, index int) bool {
	for depth := ancestryDepth; tree != nil; depth-- {
		if depth == 0 {
			return tree.bits&(uint64(1)<<(index&63)) != 0
		}
		if index&(1<<(depth+5)) == 0 {
			tree = tree.left
		} else {
			tree = tree.right
		}
	}
	return false
}

type historyPosition struct {
	Batch   int
	Payload int
}

type historyBasis struct {
	Ancestors *historyAncestry
	Current   historyPosition
}

func (basis historyBasis) contains(position historyPosition) bool {
	if position.Batch == basis.Current.Batch {
		return position.Payload < basis.Current.Payload
	}
	return hasAncestor(basis.Ancestors, position.Batch)
}

func removedAt(removals []historyPosition, basis historyBasis) bool {
	for _, removal := range removals {
		if basis.contains(removal) {
			return true
		}
	}
	return false
}
