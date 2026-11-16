package server

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

const Version = "0.2.0"

type Options struct {
	Root     string
	MaxFrame int
	// Sandbox is the deployment-provided argv prefix for isolated
	// environments. It is never supplied by a tool call and is never evaluated
	// by a shell. An empty prefix makes the isolated profile unavailable.
	Sandbox []string
	// NetAllow is an optional deployment egress allowlist. Entries are CIDR
	// blocks or domain suffixes; an empty list allows every target.
	NetAllow []string
}

type Env struct {
	ID        string `json:"env_id"`
	Name      string `json:"name"`
	Workspace string `json:"workspace"`
	Profile   string `json:"profile"`
	// Isolation names how this environment is enforced today: "none" for
	// dev/host, "launcher" for isolated.
	Isolation string `json:"isolation"`
}

// launchArgv wraps argv with the deployment sandbox when the environment is
// isolated. The command arguments stay distinct; no shell interpolation is
// performed. Callers must treat an error as a failed launch, never as a cue to
// run the command directly.
func (e *Env) launchArgv(argv []string, sandbox []string) ([]string, error) {
	if len(argv) == 0 || argv[0] == "" {
		return nil, errors.New("launch has no command")
	}
	for _, arg := range argv {
		if strings.IndexByte(arg, 0) >= 0 {
			return nil, errors.New("launch argument contains a NUL byte")
		}
	}
	switch e.Profile {
	case "dev", "host":
		return argv, nil
	case "isolated":
		if strings.HasPrefix(argv[0], "-") {
			return nil, errors.New("isolated command must not start with '-'")
		}
		if len(sandbox) == 0 || sandbox[0] == "" {
			return nil, errors.New("isolated profile has no sandbox launcher")
		}
		launch := make([]string, 0, len(sandbox)+len(argv))
		launch = append(launch, sandbox...)
		launch = append(launch, argv...)
		return launch, nil
	default:
		return nil, errors.New("unknown environment profile")
	}
}

func (s *Server) sandboxConfigured() bool {
	return len(s.opts.Sandbox) > 0 && s.opts.Sandbox[0] != ""
}

func (s *Server) isolatedAvailable() bool {
	return s.transport != "embedded-stdio" && s.sandboxConfigured()
}

// profiles lists the profiles this deployment accepts. The embedded transport
// shares the kernel's uid and only accepts dev. Isolated confinement itself is
// delegated to the configured launcher; this list only says when it is
// available, not that envd implements a sandbox.
func (s *Server) profiles() []string {
	if s.transport == "embedded-stdio" {
		return []string{"dev"}
	}
	profiles := []string{"dev", "host"}
	if s.isolatedAvailable() {
		profiles = append(profiles, "isolated")
	}
	return profiles
}

type Server struct {
	opts       Options
	mu         sync.Mutex
	envs       map[string]*Env
	sessions   map[string]*Session
	transport  string
	dataPlane  bool
	netRules   []netRule
	netRuleErr error
}

func New(opts Options) *Server {
	if opts.MaxFrame <= 0 {
		opts.MaxFrame = protocol.EmbeddedMaxFrame
	}
	opts.Sandbox = NormalizeSandbox(opts.Sandbox)
	rules, ruleErr := parseNetAllow(opts.NetAllow)
	return &Server{
		opts:       opts,
		envs:       make(map[string]*Env),
		sessions:   make(map[string]*Session),
		transport:  "embedded-stdio",
		netRules:   rules,
		netRuleErr: ruleErr,
	}
}

// NormalizeSandbox stores an absolute launcher path. exec.Cmd resolves a
// relative Path after changing to the environment workspace, which would let a
// client-writable directory supply the launcher.
func NormalizeSandbox(argv []string) []string {
	if len(argv) == 0 {
		return nil
	}
	normalized := append([]string(nil), argv...)
	if resolved, err := exec.LookPath(normalized[0]); err == nil {
		if absolute, err := filepath.Abs(resolved); err == nil {
			normalized[0] = absolute
		}
	}
	return normalized
}

// RunEmbedded serves the frame protocol over stdin/stdout. It is the only
// transport implemented in M0; service mode (MCP over HTTP) follows.
func RunEmbedded(ctx context.Context, opts Options) error {
	_ = ctx
	if len(opts.Sandbox) > 0 {
		return errors.New("--sandbox-command requires service mode")
	}
	if err := ValidateSandbox(opts.Sandbox, opts.Root); err != nil {
		return err
	}
	if err := ValidateNetAllow(opts.NetAllow); err != nil {
		return err
	}
	return New(opts).serve(os.Stdin, os.Stdout)
}

func (s *Server) serve(in io.Reader, out io.Writer) error {
	defer s.closeAllSessions()
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
	// A tool result that does not fit one control frame must become a small
	// error, never a frame-write failure: the latter tears down the embedded
	// transport and every session in it. Tools also have per-result budgets;
	// this is the last line of defence.
	if encoded, err := json.Marshal(resp); err != nil || len(encoded)+protocol.HeaderLen > s.opts.MaxFrame {
		resp = rpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Error: &rpcError{
				Code: -32005,
				Message: "response exceeds the control frame limit; narrow the request " +
					"(fewer targets/ports, a smaller max_bytes, a shorter dump)",
			},
		}
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
