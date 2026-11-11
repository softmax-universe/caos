package main

import (
	"context"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/vmihailenco/msgpack/v5"
)

func TestHistorySnapshotUsesScopedSocketAndPreservesEvents(t *testing.T) {
	actor, history := uuidFixture(16), uuidFixture(0)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/history/"+history.String()+"/connect" ||
			r.URL.Query().Get("actor_id") != actor.String() ||
			r.Header.Get("Authorization") != "Bearer test-token" {
			t.Error("wrong history socket scope or authentication")
		}
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		for _, test := range []struct {
			tag     uint64
			request string
			reply   string
		}{
			{6, "93c410000102030405060708090a0b0c0d0e0f" + historyIDWire + "90",
				"9291c402dead90"},
			{27, "92c410000102030405060708090a0b0c0d0e0f91c402dead",
				"9202930191" + historyBatchWire + "90"},
		} {
			kind, data, err := conn.Read(ctx)
			if err != nil {
				t.Error(err)
				return
			}
			fields, err := decodeFields(data, 2, 2)
			if err != nil || kind != websocket.MessageBinary {
				t.Error("invalid history request envelope")
				return
			}
			request, err := decodeVariant(fields[1])
			if err != nil || request.Tag != test.tag ||
				hex.EncodeToString(request.Payload) != test.request {
				t.Error("incorrect history request payload")
				return
			}
			if err := conn.Write(ctx, websocket.MessageBinary, []byte{4}); err != nil {
				t.Error(err)
				return
			}
			response, err := encodeWire([]any{5, []any{
				fields[0], []any{test.tag, msgpack.RawMessage(turnFixture(t, test.reply))},
			}})
			if err != nil {
				t.Error(err)
				return
			}
			if err := conn.Write(ctx, websocket.MessageBinary, response); err != nil {
				t.Error(err)
				return
			}
		}
	}))
	defer server.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	snapshot, err := fetchHistorySnapshot(ctx, strings.Replace(server.URL, "http:", "ws:", 1)+"/api/connect",
		"test-token", actor, threadID{History: history, Timeline: historyIDFixture()})
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.History != history || snapshot.Timeline != historyIDFixture() ||
		snapshot.Download.CoveredBatchCount != 1 || len(snapshot.Download.Batches) != 1 ||
		len(snapshot.Notifications) != 2 || snapshot.Notifications[0].Tag != 4 ||
		snapshot.Notifications[1].Tag != 4 {
		t.Fatal("history records or ordered notifications were lost")
	}
}

func TestHistorySnapshotRejectsTurnDispatch(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		if _, _, err := conn.Read(ctx); err != nil {
			return
		}
		// Even an unsolicited unit tag must not turn a reader into an executor.
		conn.Write(ctx, websocket.MessageBinary, []byte{18})
	}))
	defer server.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := fetchHistorySnapshot(ctx, strings.Replace(server.URL, "http:", "ws:", 1)+"/connect",
		"test-token", uuidFixture(16), threadID{History: uuidFixture(0), Timeline: historyIDFixture()}); err == nil {
		t.Fatal("history reader accepted a turn dispatch")
	}
}

func TestHistoryFetchValidatesBeforeReadingCredentials(t *testing.T) {
	for _, args := range [][]string{
		{"fetch-history"},
		{"fetch-history", "--output-file=" + t.TempDir() + "/snapshot.json", "--thread-id=invalid"},
	} {
		err := run(context.Background(), args)
		if err == nil || strings.Contains(err.Error(), "credential") {
			t.Fatalf("history options were not validated before credentials: %v", err)
		}
	}
}
