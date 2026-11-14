package server

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"sync"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

const Version = "0.1.0"

type Options struct {
	Root     string
	MaxFrame int
}

type Env struct {
	ID        string `json:"env_id"`
	Name      string `json:"name"`
	Workspace string `json:"workspace"`
	Profile   string `json:"profile"`
}

type Server struct {
	opts     Options
	mu       sync.Mutex
	envs     map[string]*Env
	sessions map[string]*Session
}

func New(opts Options) *Server {
	if opts.MaxFrame <= 0 {
		opts.MaxFrame = protocol.EmbeddedMaxFrame
	}
	return &Server{
		opts:     opts,
		envs:     make(map[string]*Env),
		sessions: make(map[string]*Session),
	}
}

// RunEmbedded serves the frame protocol over stdin/stdout. It is the only
// transport implemented in M0; service mode (MCP over HTTP) follows.
func RunEmbedded(ctx context.Context, opts Options) error {
	_ = ctx
	return New(opts).serve(os.Stdin, os.Stdout)
}

func (s *Server) serve(in io.Reader, out io.Writer) error {
	reader := bufio.NewReader(in)

	first, err := protocol.ReadFrame(reader, s.opts.MaxFrame)
	if err != nil {
		return fmt.Errorf("read hello: %w", err)
	}
	if first.Kind != protocol.KindHello {
		return fmt.Errorf("expected HELLO, got %s", first.Kind)
	}
	var hello struct {
		Protocol struct {
			Min int `json:"min"`
			Max int `json:"max"`
		} `json:"protocol"`
		AgentID string `json:"agent_id"`
	}
	if err := json.Unmarshal(first.Payload, &hello); err != nil {
		return fmt.Errorf("hello payload: %w", err)
	}
	if hello.Protocol.Max < protocol.Version {
		return fmt.Errorf("client protocol max %d is below %d", hello.Protocol.Max, protocol.Version)
	}

	ack := map[string]any{
		"protocol":        protocol.Version,
		"server":          "envd " + Version,
		"max_frame":       s.opts.MaxFrame,
		"credits_initial": 65536,
		"resume":          []any{},
	}
	if err := s.writeJSON(out, protocol.KindHelloAck, 0, ack); err != nil {
		return err
	}

	for {
		frame, err := protocol.ReadFrame(reader, s.opts.MaxFrame)
		if err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return fmt.Errorf("read frame: %w", err)
		}

		switch frame.Kind {
		case protocol.KindControl:
			if err := s.handleControl(out, frame.Payload); err != nil {
				return err
			}
		case protocol.KindPing:
			if err := s.writeFrame(out, protocol.Frame{Kind: protocol.KindPong}); err != nil {
				return err
			}
		case protocol.KindClose:
			return nil
		default:
			errPayload := map[string]any{
				"code":      12,
				"name":      "UNSUPPORTED",
				"category":  "unsupported",
				"retryable": false,
				"message":   fmt.Sprintf("frame kind %s is not supported in M0", frame.Kind),
			}
			if err := s.writeJSON(out, protocol.KindError, frame.StreamID, errPayload); err != nil {
				return err
			}
		}
	}
}

func (s *Server) handleControl(out io.Writer, payload []byte) error {
	var req rpcRequest
	if err := json.Unmarshal(payload, &req); err != nil {
		return s.writeJSON(out, protocol.KindControl, 0, rpcResponse{
			JSONRPC: "2.0",
			ID:      json.RawMessage("null"),
			Error:   &rpcError{Code: -32700, Message: "parse error"},
		})
	}

	resp, ok := s.dispatch(req)
	if !ok {
		return nil
	}
	return s.writeJSON(out, protocol.KindControl, 0, resp)
}

func (s *Server) writeJSON(out io.Writer, kind protocol.Kind, streamID uint32, value any) error {
	payload, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return s.writeFrame(out, protocol.Frame{
		Kind:     kind,
		StreamID: streamID,
		Payload:  payload,
	})
}

func (s *Server) writeFrame(out io.Writer, frame protocol.Frame) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return protocol.WriteFrame(out, frame, s.opts.MaxFrame)
}
