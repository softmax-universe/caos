package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"path"
	"time"

	"github.com/coder/websocket"
)

const defaultRemoteURL = "wss://delta.zed.dev/connect"

func connectionURL(remoteURL string, actor uuid, history *uuid) (string, error) {
	u, err := url.Parse(remoteURL)
	if err != nil || u.Host == "" || u.User != nil || u.Fragment != "" {
		return "", errors.New("invalid Delta WebSocket URL")
	}
	loopback := u.Hostname() == "127.0.0.1" || u.Hostname() == "::1" || u.Hostname() == "localhost"
	if u.Scheme != "wss" && !(u.Scheme == "ws" && loopback) {
		return "", errors.New("Delta WebSocket must use TLS")
	}
	query := u.Query()
	if query.Has("actor_id") {
		return "", errors.New("WebSocket URL must not override the actor identity")
	}
	query.Set("actor_id", actor.String())
	u.RawQuery = query.Encode()
	if history != nil {
		// The control socket cannot serve history requests. The public client
		// uses history/<UUID>/connect relative to the same service prefix.
		u.Path = path.Join(path.Dir(u.Path), "history", history.String(), "connect")
		u.RawPath = ""
	}
	return u.String(), nil
}

func connectRemote(ctx context.Context, remoteURL, token string, actor uuid, history *uuid) (*websocket.Conn, string, error) {
	if token == "" {
		return nil, "", errors.New("Delta service token is required")
	}
	target, err := connectionURL(remoteURL, actor, history)
	if err != nil {
		return nil, "", err
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: 15 * time.Second, KeepAlive: 30 * time.Second}).DialContext
	transport.TLSHandshakeTimeout = 15 * time.Second
	transport.ResponseHeaderTimeout = 15 * time.Second
	headers := http.Header{
		"Authorization":    {"Bearer " + token},
		"X-Delta-Protocol": {deltaProtocol},
		"User-Agent":       {"caos-delta-connector"},
	}
	conn, response, err := websocket.Dial(ctx, target, &websocket.DialOptions{
		HTTPHeader: headers,
		HTTPClient: &http.Client{
			Transport: transport,
			CheckRedirect: func(_ *http.Request, _ []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
	})
	if err != nil {
		transport.CloseIdleConnections()
		if response != nil {
			return nil, "", fmt.Errorf("Delta WebSocket upgrade failed with HTTP %d", response.StatusCode)
		}
		return nil, "", errors.New("Delta WebSocket connection failed")
	}
	conn.SetReadLimit(64 << 20)
	return conn, response.Header.Get("X-Delta-Connection-Id"), nil
}

type probeResult struct {
	ConnectionID         string
	ResponseTag          uint64
	RegistrationVerified bool
}

func probeRemote(ctx context.Context, remoteURL, token string, actor uuid, registrationName *string) (probeResult, error) {
	var result probeResult
	conn, connectionID, err := connectRemote(ctx, remoteURL, token, actor, nil)
	if err != nil {
		return result, err
	}
	s := newSession(ctx, conn)
	drained := make(chan struct{})
	go func() {
		defer close(drained)
		for message := range s.events {
			if message.Tag == 18 {
				s.Abort(errors.New("a diagnostic connection must not receive a turn"))
				return
			}
		}
	}()
	defer func() {
		s.Close()
		<-drained
	}()
	result.ConnectionID = connectionID
	body, err := s.Call(ctx, heartbeatRequest())
	if err != nil {
		return result, err
	}
	if body.Tag != 33 {
		return result, errors.New("Delta did not accept the protocol heartbeat")
	}
	result.ResponseTag = body.Tag
	if registrationName == nil {
		return result, nil
	}
	// This tests registration, then clears it. It must not advertise an
	// unfinished executor as an available machine after this probe exits.
	for _, name := range []*string{registrationName, nil} {
		body, err := s.Call(ctx, machineRunnerRequest(name))
		if err != nil {
			return result, err
		}
		if body.Tag != 51 {
			return result, fmt.Errorf("unexpected machine-runner response tag %d", body.Tag)
		}
		update, err := decodeVariant(body.Payload)
		if err != nil || update.Tag != 0 {
			return result, errors.New("Delta refused the machine-runner update")
		}
	}
	result.RegistrationVerified = true
	return result, nil
}
