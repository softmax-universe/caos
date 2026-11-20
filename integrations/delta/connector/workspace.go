package main

import (
	"bytes"
	"context"
	"errors"
	"sort"
)

type workspaceReference struct {
	Label      uuid
	Index      uint64
	Repository []byte
	History    uuid
	Timeline   deltaID
	Version    []deltaID
	Worktree   deltaID
}

func threadWorkspaces(state *replayState) ([]workspaceReference, error) {
	values := state.Maps[fieldObject(43)]
	if values == nil {
		return nil, nil
	}
	var result []workspaceReference
	for key := range values.entries {
		value := values.get(key, state.basis)
		if !value.Present {
			continue
		}
		fields, err := decodeFields(value.Bytes, 2, 2)
		if err != nil {
			return nil, err
		}
		status, err := decodeVariant(fields[1])
		if err != nil {
			return nil, err
		}
		if status.Tag == 2 {
			continue // Removed attachment.
		}
		if status.Tag != 1 {
			return nil, errors.New("workspace attachment is not resolved")
		}
		var reference workspaceReference
		if reference.Label, err = decodeUUID([]byte(key)); err != nil {
			return nil, err
		}
		if reference.Index, err = decodeUint64(fields[0]); err != nil {
			return nil, err
		}
		resolved, err := decodeFields(status.Payload, 2, 4)
		if err != nil {
			return nil, err
		}
		if reference.Repository, err = decodeBytes(resolved[0]); err != nil {
			return nil, err
		}
		stub, err := decodeFields(resolved[1], 4, 4)
		if err != nil {
			return nil, err
		}
		if reference.History, err = decodeUUID(stub[0]); err != nil {
			return nil, err
		}
		if reference.Timeline, err = decodeDeltaID(stub[1]); err != nil {
			return nil, err
		}
		if reference.Version, err = decodeHistoryVersion(stub[2]); err != nil {
			return nil, err
		}
		root, err := decodeBytes(stub[3])
		if err != nil {
			return nil, err
		}
		if reference.Worktree, err = decodeDeltaID(root); err != nil {
			return nil, err
		}
		result = append(result, reference)
	}
	sort.Slice(result, func(i, j int) bool {
		if result[i].Index != result[j].Index {
			return result[i].Index < result[j].Index
		}
		return bytes.Compare(result[i].Label[:], result[j].Label[:]) < 0
	})
	return result, nil
}

type initialWorkspace struct {
	Tree       workspaceTree
	Repository []byte
	Head       *gitOID
}

// Follow only descriptors obtained from an already authorized thread. The
// descriptor's pinned frontier, not the workspace's latest tip, defines its files.
func fetchInitialWorkspace(ctx context.Context, remoteURL, token string, actor uuid, reference workspaceReference) (initialWorkspace, error) {
	snapshot, err := fetchHistorySnapshot(ctx, remoteURL, token, actor, threadID{
		History: reference.History, Timeline: reference.Timeline,
	})
	if err != nil {
		return initialWorkspace{}, err
	}
	return reconstructInitialWorkspace(reference, snapshot.Download)
}

func reconstructInitialWorkspace(reference workspaceReference, download historyDownload) (initialWorkspace, error) {
	var workspace initialWorkspace
	if len(download.ReplicationPacks) != 0 {
		return workspace, errors.New("workspace replication-pack download is not implemented")
	}
	if int(download.CoveredBatchCount) != len(download.Batches) {
		return workspace, errors.New("workspace download does not cover its declared batches")
	}
	plan, _, err := planHistory(download.Batches, reference.Version)
	if err != nil {
		return workspace, err
	}
	found := false
	for _, item := range plan {
		for index, payload := range item.Batch.Payloads {
			if payload.Tag == 12 {
				continue
			}
			if payload.Tag != 8 {
				return workspace, errors.New("incremental workspace-delta replay is not implemented")
			}
			id, err := payloadID(item.Batch, index)
			if err != nil {
				return workspace, err
			}
			if id != reference.Worktree {
				continue
			}
			if found {
				return workspace, errors.New("duplicate workspace creation")
			}
			fields, err := decodeFields(payload.Payload, 3, 3)
			if err != nil {
				return workspace, err
			}
			workspace.Repository, err = decodeBytes(fields[0])
			if err != nil || !bytes.Equal(workspace.Repository, reference.Repository) {
				return workspace, errors.New("workspace repository identity mismatch")
			}
			head, err := decodeOption(fields[1], func(raw []byte) (gitOID, error) {
				fields, err := decodeFields(raw, 1, 4)
				if err != nil {
					return gitOID{}, err
				}
				return decodeGitOID(fields[0])
			})
			if err != nil {
				return workspace, err
			}
			workspace.Head = head
			entries, err := decodeSequence(fields[2], maxHistoryItems)
			if err != nil {
				return workspace, err
			}
			workspace.Tree, err = buildWorkspaceTree(entries)
			if err != nil {
				return workspace, err
			}
			found = true
		}
	}
	if !found {
		return workspace, errors.New("workspace creation is missing from its pinned history")
	}
	return workspace, nil
}
