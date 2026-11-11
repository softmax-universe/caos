package main

import (
	"context"
	"errors"
	"fmt"

	"github.com/vmihailenco/msgpack/v5"
)

type historySnapshot struct {
	History  uuid
	Timeline deltaID
	Download historyDownload
	// Timeline metadata and notifications remain wire values until the state
	// bridge can interpret them. Keeping them is not applying or replaying them.
	Timelines     []msgpack.RawMessage
	Notifications []wireVariant
}

// fetchHistorySnapshot is a bounded, read-only download. It neither subscribes
// to a timeline nor admits turns. A snapshot containing replication-pack
// references is not complete until those packs have also been downloaded.
func fetchHistorySnapshot(ctx context.Context, remoteURL, token string, actor uuid, thread threadID) (snapshot historySnapshot, err error) {
	request, err := historySinceRequest(thread, nil)
	if err != nil {
		return snapshot, err
	}
	conn, _, err := connectRemote(ctx, remoteURL, token, actor, &thread.History)
	if err != nil {
		return snapshot, err
	}
	s := newSession(ctx, conn)
	drained := make(chan struct{})
	var eventError error
	var notifications []wireVariant
	go func() {
		defer close(drained)
		size := 0
		for event := range s.events {
			size += len(event.Payload)
			if event.Tag == 18 || len(notifications) >= 256 || size > 1<<20 {
				eventError = errors.New("unexpected turn or excessive notifications during history download")
				s.Abort(eventError)
				return
			}
			notifications = append(notifications, event)
		}
	}()
	defer func() {
		s.Close()
		<-drained
		snapshot.Notifications = notifications
		if err == nil {
			err = eventError
		}
	}()
	snapshot.History, snapshot.Timeline = thread.History, thread.Timeline
	index, err := s.Call(ctx, request)
	if err != nil {
		return snapshot, err
	}
	if index.Tag != 6 {
		return snapshot, fmt.Errorf("Delta history index was not accepted (response %d)", index.Tag)
	}
	fields, err := decodeFields(index.Payload, 2, 2)
	if err != nil {
		return snapshot, err
	}
	snapshot.Timelines, err = decodeSequence(fields[1], maxHistoryItems)
	if err != nil {
		return snapshot, err
	}
	request, err = historyDeltasRequest(thread.History, fields[0])
	if err != nil {
		return snapshot, err
	}
	response, err := s.Call(ctx, request)
	if err != nil {
		return snapshot, err
	}
	if response.Tag != 27 {
		return snapshot, fmt.Errorf("Delta history deltas were not accepted (response %d)", response.Tag)
	}
	snapshot.Download, err = decodeHistoryDownload(response.Payload)
	return snapshot, err
}
