package main

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/vmihailenco/msgpack/v5"
)

func testSession(t *testing.T, serve func(context.Context, *websocket.Conn)) (*session, context.Context) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		serve(ctx, conn)
	}))
	conn, _, err := connectRemote(ctx, strings.Replace(server.URL, "http:", "ws:", 1),
		"test-session-token", uuidFixture(0), nil)
	if err != nil {
		cancel()
		server.Close()
		t.Fatal(err)
	}
	s := newSession(ctx, conn)
	t.Cleanup(func() {
		s.Close()
		cancel()
		server.Close()
	})
	return s, ctx
}

func readTestRequest(ctx context.Context, conn *websocket.Conn) (uint32, uint64, error) {
	_, data, err := conn.Read(ctx)
	if err != nil {
		return 0, 0, err
	}
	fields, err := decodeFields(data, 2, 2)
	if err != nil {
		return 0, 0, err
	}
	var id uint32
	if err := msgpack.Unmarshal(fields[0], &id); err != nil {
		return 0, 0, err
	}
	body, err := decodeVariant(fields[1])
	return id, body.Tag, err
}

func respond(ctx context.Context, conn *websocket.Conn, id uint32, tag uint64) error {
	data, err := encodeWire([]any{uint64(5), []any{id, tag}})
	if err != nil {
		return err
	}
	return conn.Write(ctx, websocket.MessageBinary, data)
}

func TestConcurrentRPCAndOrderedNotifications(t *testing.T) {
	s, ctx := testSession(t, func(ctx context.Context, conn *websocket.Conn) {
		first, firstTag, err := readTestRequest(ctx, conn)
		if err != nil {
			t.Error(err)
			return
		}
		second, secondTag, err := readTestRequest(ctx, conn)
		if err != nil {
			t.Error(err)
			return
		}
		if first == second {
			t.Error("request ids reused")
			return
		}
		for _, tag := range []byte{4, 6} {
			if err := conn.Write(ctx, websocket.MessageBinary, []byte{tag}); err != nil {
				t.Error(err)
				return
			}
		}
		if err := respond(ctx, conn, second, secondTag); err != nil {
			t.Error(err)
		}
		if err := respond(ctx, conn, first, firstTag); err != nil {
			t.Error(err)
		}
	})
	notifications := make(chan uint64, 2)
	go func() {
		for event := range s.events {
			notifications <- event.Tag
		}
	}()
	type checkedResult struct {
		want uint64
		rpcResult
	}
	results := make(chan checkedResult, 2)
	for _, request := range []clientRequest{heartbeatRequest(), machineRunnerRequest(nil)} {
		go func() {
			body, err := s.Call(ctx, request)
			results <- checkedResult{request.tag, rpcResult{body, err}}
		}()
	}
	for range 2 {
		result := <-results
		if result.err != nil || result.body.Tag != result.want {
			t.Fatalf("out-of-order response was misrouted: %v", result.err)
		}
	}
	if <-notifications != 4 || <-notifications != 6 {
		t.Fatal("notifications were dropped or reordered")
	}
}

func TestDisconnectLeavesRequestOutcomeUncertain(t *testing.T) {
	s, ctx := testSession(t, func(ctx context.Context, conn *websocket.Conn) {
		if _, _, err := readTestRequest(ctx, conn); err != nil {
			t.Error(err)
			return
		}
		conn.Close(websocket.StatusPolicyViolation, "private-server-reason")
	})
	_, err := s.Call(ctx, heartbeatRequest())
	if !errors.Is(err, errRPCUncertain) || strings.Contains(err.Error(), "private-server-reason") {
		t.Fatalf("incorrect or unsafe disconnect outcome: %v", err)
	}
}

func TestCanceledRPCDoesNotConsumeNextResponse(t *testing.T) {
	received := make(chan struct{})
	release := make(chan struct{})
	s, ctx := testSession(t, func(ctx context.Context, conn *websocket.Conn) {
		first, _, err := readTestRequest(ctx, conn)
		if err != nil {
			t.Error(err)
			return
		}
		close(received)
		select {
		case <-release:
		case <-ctx.Done():
			return
		}
		if err := respond(ctx, conn, first, 33); err != nil {
			t.Error(err)
			return
		}
		second, _, err := readTestRequest(ctx, conn)
		if err != nil {
			t.Error(err)
			return
		}
		if err := respond(ctx, conn, second, 33); err != nil {
			t.Error(err)
		}
	})
	callCtx, cancel := context.WithCancel(ctx)
	result := make(chan error, 1)
	go func() {
		_, err := s.Call(callCtx, heartbeatRequest())
		result <- err
	}()
	select {
	case <-received:
	case <-ctx.Done():
		t.Fatal("request was never sent")
	}
	cancel()
	if err := <-result; !errors.Is(err, errRPCUncertain) {
		t.Fatalf("sent request incorrectly treated as canceled execution: %v", err)
	}
	close(release)
	body, err := s.Call(ctx, heartbeatRequest())
	if err != nil || body.Tag != 33 {
		t.Fatalf("late response corrupted the next request: %v", err)
	}
}
