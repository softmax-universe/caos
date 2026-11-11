package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"time"

	"github.com/coder/websocket"
	"github.com/vmihailenco/msgpack/v5"
)

const defaultRemoteURL = "wss://delta.zed.dev/connect"

func connectionURL(remoteURL string, actor uuid) (string, error) {
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
	return u.String(), nil
}

func connectRemote(ctx context.Context, remoteURL, token string, actor uuid) (*websocket.Conn, string, error) {
	if token == "" {
		return nil, "", errors.New("Delta service token is required")
	}
	target, err := connectionURL(remoteURL, actor)
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

func exchangeProbe(ctx context.Context, conn *websocket.Conn, requestID uint32, request []byte) (wireVariant, error) {
	if err := conn.Write(ctx, websocket.MessageBinary, request); err != nil {
		return wireVariant{}, errors.New("could not send protocol request")
	}
	for {
		kind, data, err := conn.Read(ctx)
		if err != nil {
			return wireVariant{}, errors.New("Delta closed before acknowledging the protocol request")
		}
		if kind != websocket.MessageBinary {
			return wireVariant{}, errors.New("expected a binary Delta message")
		}
		message, err := decodeVariant(data)
		if err != nil {
			return wireVariant{}, err
		}
		if message.Tag == 18 {
			return wireVariant{}, errors.New("a diagnostic connection must not receive a turn")
		}
		if message.Tag != 5 {
			continue
		}
		fields, err := decodeFields(message.Payload, 2, 2)
		if err != nil {
			return wireVariant{}, errors.New("invalid protocol response")
		}
		var responseID uint32
		if err := msgpack.Unmarshal(fields[0], &responseID); err != nil || responseID != requestID {
			continue
		}
		body, err := decodeVariant(fields[1])
		if err != nil {
			return wireVariant{}, errors.New("invalid protocol response body")
		}
		return body, nil
	}
}

func probeRemote(ctx context.Context, remoteURL, token string, actor uuid, registrationName *string) (probeResult, error) {
	var result probeResult
	conn, connectionID, err := connectRemote(ctx, remoteURL, token, actor)
	if err != nil {
		return result, err
	}
	defer conn.CloseNow()
	result.ConnectionID = connectionID
	heartbeat, err := encodeHeartbeat(1)
	if err != nil {
		return result, err
	}
	body, err := exchangeProbe(ctx, conn, 1, heartbeat)
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
	for i, name := range []*string{registrationName, nil} {
		requestID := uint32(i + 2)
		request, err := encodeMachineRunner(requestID, name)
		if err != nil {
			return result, err
		}
		body, err := exchangeProbe(ctx, conn, requestID, request)
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
