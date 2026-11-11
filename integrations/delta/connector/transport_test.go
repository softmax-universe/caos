package main

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestAuthenticatedProtocolExchange(t *testing.T) {
	actor := uuidFixture(0)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/connect" || r.URL.Query().Get("actor_id") != actor.String() {
			t.Error("incorrect connection URL or identity")
		}
		if r.Header.Get("Authorization") != "Bearer test-service-token" ||
			r.Header.Get("X-Delta-Protocol") != "delta-v49" {
			t.Error("incorrect Delta handshake")
		}
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		kind, data, err := conn.Read(ctx)
		if err != nil {
			t.Error(err)
			return
		}
		request, err := decodeFields(data, 2, 2)
		if err != nil || kind != websocket.MessageBinary || len(request[0]) != 1 || request[0][0] != 1 {
			t.Error("incorrect heartbeat request")
			return
		}
		// An unrelated notification must not be mistaken for the RPC response.
		if err := conn.Write(ctx, websocket.MessageBinary, []byte{0x04}); err != nil {
			t.Error(err)
			return
		}
		if err := conn.Write(ctx, websocket.MessageBinary, []byte{0x92, 0x05, 0x92, 0x01, 0x21}); err != nil {
			t.Error(err)
		}
	}))
	defer server.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	result, err := probeRemote(ctx, strings.Replace(server.URL, "http:", "ws:", 1)+"/connect",
		"test-service-token", actor, nil)
	if err != nil || result.ResponseTag != 33 {
		t.Fatalf("protocol exchange failed: %v", err)
	}
}

func TestWebSocketUpgradeFailureRedactsCredentials(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		w.Write([]byte("do-not-print-this-response"))
	}))
	defer server.Close()
	conn, _, err := connectRemote(context.Background(),
		strings.Replace(server.URL, "http:", "ws:", 1), "test-secret-token", uuidFixture(0))
	if conn != nil || err == nil || strings.Contains(err.Error(), "test-secret-token") ||
		strings.Contains(err.Error(), "do-not-print-this-response") {
		t.Fatalf("unsafe failed handshake: %v", err)
	}
}

func TestConnectionIdentityAndTLS(t *testing.T) {
	for _, target := range []string{
		"ws://example.com/connect", "wss://user:password@example.com/connect",
		"wss://example.com/connect#fragment", "wss://example.com/connect?actor_id=other",
	} {
		if _, err := connectionURL(target, uuidFixture(0)); err == nil {
			t.Errorf("accepted unsafe connection URL: %s", target)
		}
	}
}
