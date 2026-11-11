package main

import (
	"context"
	"errors"
	"fmt"
	"math"
	"sync"

	"github.com/coder/websocket"
)

var (
	errSessionClosed = errors.New("Delta session is closed")
	errRPCUncertain  = errors.New("Delta request outcome is unknown; reconcile remote state before retrying")
)

type rpcResult struct {
	body wireVariant
	err  error
}

// session owns the one WebSocket reader. Notifications retain wire order and
// apply backpressure rather than being dropped while an RPC is pending.
// The state owner must drain events independently of any blocking Call.
type session struct {
	conn   *websocket.Conn
	ctx    context.Context
	cancel context.CancelFunc
	events chan wireVariant
	done   chan struct{}

	mu      sync.Mutex
	nextID  uint64
	pending map[uint32]chan rpcResult
	closed  bool
	err     error
}

func newSession(ctx context.Context, conn *websocket.Conn) *session {
	ctx, cancel := context.WithCancel(ctx)
	s := &session{
		conn: conn, ctx: ctx, cancel: cancel,
		events: make(chan wireVariant), done: make(chan struct{}),
		nextID: 1, pending: make(map[uint32]chan rpcResult),
	}
	go s.read()
	return s
}

func (s *session) finish(reason error) {
	s.mu.Lock()
	s.closed = true
	if s.err == nil {
		s.err = reason
	}
	for id, response := range s.pending {
		response <- rpcResult{err: fmt.Errorf("%w: %v", errRPCUncertain, s.err)}
		delete(s.pending, id)
	}
	s.mu.Unlock()
	s.cancel()
	s.conn.CloseNow()
	close(s.events)
	close(s.done)
}

func (s *session) read() {
	reason := errSessionClosed
	defer func() { s.finish(reason) }()
	for {
		kind, data, err := s.conn.Read(s.ctx)
		if err != nil {
			if s.ctx.Err() != nil {
				reason = s.ctx.Err()
			} else {
				reason = fmt.Errorf("Delta WebSocket closed (code %d)", websocket.CloseStatus(err))
			}
			return
		}
		if kind != websocket.MessageBinary {
			reason = errors.New("expected a binary Delta protocol frame")
			return
		}
		message, err := decodeVariant(data)
		if err != nil {
			reason = err
			return
		}
		if message.Tag != 5 {
			select {
			case s.events <- message:
			case <-s.ctx.Done():
				return
			}
			continue
		}
		id, body, err := decodeResponse(message)
		if err != nil {
			reason = err
			return
		}
		s.mu.Lock()
		response := s.pending[id]
		delete(s.pending, id)
		s.mu.Unlock()
		if response != nil {
			response <- rpcResult{body: body}
		}
	}
}

func (s *session) Call(ctx context.Context, request clientRequest) (wireVariant, error) {
	if err := ctx.Err(); err != nil {
		return wireVariant{}, err // Nothing has been sent.
	}
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return wireVariant{}, errSessionClosed
	}
	if s.nextID > math.MaxUint32 {
		s.mu.Unlock()
		return wireVariant{}, errors.New("Delta request ids exhausted")
	}
	id := uint32(s.nextID)
	s.nextID++
	response := make(chan rpcResult, 1)
	s.pending[id] = response
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.pending, id)
		s.mu.Unlock()
	}()
	data, err := request.encode(id)
	if err != nil {
		return wireVariant{}, err
	}
	if err := s.conn.Write(ctx, websocket.MessageBinary, data); err != nil {
		return wireVariant{}, errRPCUncertain
	}
	select {
	case result := <-response:
		return result.body, result.err
	case <-ctx.Done():
		// Cancellation stops waiting, not execution at the server. Prefer an
		// already received outcome; otherwise the caller must reconcile it.
		select {
		case result := <-response:
			return result.body, result.err
		default:
			return wireVariant{}, fmt.Errorf("%w: %v", errRPCUncertain, ctx.Err())
		}
	}
}

func (s *session) Close() {
	s.Abort(context.Canceled)
}

func (s *session) Abort(reason error) {
	s.mu.Lock()
	if s.err == nil && !s.closed {
		s.err = reason
	}
	s.mu.Unlock()
	s.cancel()
	s.conn.CloseNow()
	<-s.done
}
