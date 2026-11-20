package main

import (
	"bytes"
	"crypto/sha1"
	"errors"
	"fmt"
	"sort"
	"strings"

	"github.com/vmihailenco/msgpack/v5"
)

type gitOID [20]byte

type gitObject struct {
	Kind string
	Data []byte
}

func hashGitObject(kind string, data []byte) gitOID {
	hash := sha1.New()
	fmt.Fprintf(hash, "%s %d\x00", kind, len(data))
	hash.Write(data)
	var oid gitOID
	copy(oid[:], hash.Sum(nil))
	return oid
}

func decodeGitOID(data []byte) (gitOID, error) {
	var oid gitOID
	raw, err := decodeBinary(data, len(oid))
	if err == nil {
		copy(oid[:], raw)
	}
	return oid, err
}

type workspaceNode struct {
	Children map[string]*workspaceNode
	Mode     uint32
	OID      gitOID
}

type workspaceTree struct {
	Root    gitOID
	Objects map[gitOID]gitObject
}

func repositoryPath(value string) ([]string, error) {
	if value == "" {
		return nil, nil
	}
	if strings.HasPrefix(value, "/") || strings.ContainsRune(value, 0) {
		return nil, errors.New("invalid absolute or NUL-containing repository path")
	}
	parts := strings.Split(value, "/")
	for _, part := range parts {
		if part == "" || part == "." || part == ".." || part == ".git" {
			return nil, errors.New("unsafe or noncanonical repository path")
		}
	}
	return parts, nil
}

func buildWorkspaceTree(entries []msgpack.RawMessage) (workspaceTree, error) {
	result := workspaceTree{Objects: make(map[gitOID]gitObject)}
	root := &workspaceNode{Children: make(map[string]*workspaceNode), Mode: 040000}
	for _, raw := range entries {
		entry, err := decodeVariant(raw)
		if err != nil {
			return result, err
		}
		count := 1
		if entry.Tag == 1 {
			count = 3
		} else if entry.Tag == 2 {
			count = 2
		} else if entry.Tag != 0 {
			return result, errors.New("unknown repository entry variant")
		}
		fields, err := decodeFields(entry.Payload, count, count)
		if err != nil {
			return result, err
		}
		path, err := decodeString(fields[0])
		if err != nil {
			return result, err
		}
		parts, err := repositoryPath(path)
		if err != nil || len(parts) == 0 && entry.Tag != 0 {
			return result, errors.New("invalid repository entry path")
		}
		directory := root
		for index, part := range parts {
			final := index == len(parts)-1
			node := directory.Children[part]
			if !final || entry.Tag == 0 {
				if node == nil {
					node = &workspaceNode{Children: make(map[string]*workspaceNode), Mode: 040000}
					directory.Children[part] = node
				}
				if node.Mode != 040000 {
					return result, errors.New("repository directory conflicts with a file")
				}
				directory = node
				continue
			}
			if node != nil {
				return result, errors.New("duplicate or conflicting repository path")
			}
			node = &workspaceNode{}
			if entry.Tag == 1 {
				version, err := decodeVariant(fields[1])
				if err != nil || version.Tag != 0 {
					return result, errors.New("repository file-delta materialization is not implemented")
				}
				node.OID, err = decodeGitOID(version.Payload)
				if err != nil {
					return result, err
				}
				mode, err := decodeUint64(fields[2])
				if err != nil || mode > 1 {
					return result, errors.New("invalid repository file mode")
				}
				node.Mode = 0100644
				if mode == 1 {
					node.Mode = 0100755
				}
			} else {
				target, err := decodeString(fields[1])
				if err != nil || strings.ContainsRune(target, 0) {
					return result, errors.New("invalid repository symlink target")
				}
				node.Mode = 0120000
				data := []byte(target)
				node.OID = hashGitObject("blob", data)
				result.Objects[node.OID] = gitObject{Kind: "blob", Data: bytes.Clone(data)}
			}
			directory.Children[part] = node
		}
	}
	var emit func(*workspaceNode) gitOID
	emit = func(node *workspaceNode) gitOID {
		var names []string
		for name := range node.Children {
			names = append(names, name)
		}
		key := func(name string) string {
			if node.Children[name].Mode == 040000 {
				return name + "/"
			}
			return name
		}
		sort.Slice(names, func(i, j int) bool { return key(names[i]) < key(names[j]) })
		var data bytes.Buffer
		for _, name := range names {
			child := node.Children[name]
			oid := child.OID
			if child.Mode == 040000 {
				oid = emit(child)
			}
			fmt.Fprintf(&data, "%o %s\x00", child.Mode, name)
			data.Write(oid[:])
		}
		raw := bytes.Clone(data.Bytes())
		oid := hashGitObject("tree", raw)
		result.Objects[oid] = gitObject{Kind: "tree", Data: raw}
		return oid
	}
	result.Root = emit(root)
	return result, nil
}
