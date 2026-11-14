package server

import (
	"context"
	"crypto/subtle"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

// ServiceOptions configures the network transports. The control plane is MCP
// JSON-RPC over HTTP(S); the data plane is MSDP over TCP/TLS.
type ServiceOptions struct {
	Options
	Addr     string
	DataAddr string
	Token    string
	TLSCert  string
	TLSKey   string
}

func tokenEqual(left, right string) bool {
	return subtle.ConstantTimeCompare([]byte(left), []byte(right)) == 1
}

type serviceHandler struct {
	server *Server
	token  string
}

func (h serviceHandler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/healthz" {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"ok":      true,
			"server":  "envd " + Version,
			"service": true,
		})
		return
	}
	if r.URL.Path != "/mcp" {
		http.NotFound(w, r)
		return
	}
	if r.Method != http.MethodPost {
		w.Header().Set("Allow", http.MethodPost)
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	authorization := r.Header.Get("Authorization")
	const prefix = "Bearer "
	if len(authorization) <= len(prefix) ||
		!tokenEqual(authorization[:len(prefix)], prefix) ||
		!tokenEqual(authorization[len(prefix):], h.token) {
		http.Error(w, "unauthorized", http.StatusUnauthorized)
		return
	}

	body, err := io.ReadAll(io.LimitReader(r.Body, int64(h.server.opts.MaxFrame)))
	if err != nil {
		writeHTTPRPCError(w, -32700, "read request: "+err.Error())
		return
	}
	var request rpcRequest
	if err := json.Unmarshal(body, &request); err != nil {
		writeHTTPRPCError(w, -32700, "parse error")
		return
	}
	response, ok := h.server.dispatch(request)
	if !ok {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(response); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
	}
}

func writeHTTPRPCError(w http.ResponseWriter, code int, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusBadRequest)
	_ = json.NewEncoder(w).Encode(rpcResponse{
		JSONRPC: "2.0",
		ID:      json.RawMessage("null"),
		Error:   &rpcError{Code: code, Message: message},
	})
}

// RunService starts the HTTP control plane and (when DataAddr is set) the MSDP
// data plane. It returns when ctx is cancelled or a listener fails.
func RunService(ctx context.Context, opts ServiceOptions) error {
	if opts.Token == "" {
		return errors.New("a service token is required")
	}
	if opts.Addr == "" {
		return errors.New("a control listen address is required")
	}
	server := New(opts.Options)
	server.transport = "service-http"
	server.dataPlane = opts.DataAddr != ""
	tlsConfig, err := loadTLS(opts.TLSCert, opts.TLSKey)
	if err != nil {
		return err
	}

	errs := make(chan error, 2)
	go func() { errs <- server.runHTTP(ctx, opts.Addr, opts.Token, tlsConfig) }()
	if opts.DataAddr != "" {
		go func() {
			errs <- server.runData(ctx, opts.DataAddr, opts.Token, tlsConfig)
		}()
	}
	select {
	case <-ctx.Done():
		return nil
	case err := <-errs:
		return err
	}
}

func loadTLS(certFile, keyFile string) (*tls.Config, error) {
	if certFile == "" && keyFile == "" {
		return nil, nil
	}
	if certFile == "" || keyFile == "" {
		return nil, errors.New("both --tls-cert and --tls-key are required")
	}
	certificate, err := tls.LoadX509KeyPair(certFile, keyFile)
	if err != nil {
		return nil, fmt.Errorf("tls key pair: %w", err)
	}
	return &tls.Config{
		MinVersion:   tls.VersionTLS13,
		Certificates: []tls.Certificate{certificate},
	}, nil
}

func (s *Server) runHTTP(ctx context.Context, addr, token string, tlsConfig *tls.Config) error {
	httpServer := &http.Server{
		Addr:              addr,
		Handler:           serviceHandler{server: s, token: token},
		ReadHeaderTimeout: 10 * time.Second,
		MaxHeaderBytes:    1 << 20,
		TLSConfig:         tlsConfig,
	}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = httpServer.Shutdown(shutdownCtx)
	}()
	var err error
	if tlsConfig != nil {
		err = httpServer.ListenAndServeTLS("", "")
	} else {
		err = httpServer.ListenAndServe()
	}
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

func (s *Server) runData(ctx context.Context, addr, token string, tlsConfig *tls.Config) error {
	listener, err := net.Listen("tcp", addr)
	if err != nil {
		return fmt.Errorf("data listen: %w", err)
	}
	if tlsConfig != nil {
		listener = tls.NewListener(listener, tlsConfig)
	}
	go func() {
		<-ctx.Done()
		_ = listener.Close()
	}()
	for {
		conn, err := listener.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return fmt.Errorf("data accept: %w", err)
		}
		go s.handleDataConn(conn, token)
	}
}

var errDataClosed = errors.New("data connection closed by client")

func (s *Server) handleDataConn(conn net.Conn, token string) {
	defer conn.Close()
	maxFrame := s.opts.MaxFrame
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))

	hello, err := protocol.ReadFrame(conn, maxFrame)
	if err != nil {
		return
	}
	if hello.Kind != protocol.KindHello {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 12, "name": "UNSUPPORTED", "message": "expected HELLO",
		})
		return
	}
	var helloPayload struct {
		Token    string `json:"token"`
		Protocol struct {
			Min int `json:"min"`
			Max int `json:"max"`
		} `json:"protocol"`
	}
	if err := json.Unmarshal(hello.Payload, &helloPayload); err != nil ||
		!tokenEqual(helloPayload.Token, token) {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 401, "name": "UNAUTHORIZED", "message": "invalid token",
		})
		return
	}
	if helloPayload.Protocol.Max < protocol.Version {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 2, "name": "UNSUPPORTED", "message": "protocol too old",
		})
		return
	}
	if err := writeServiceFrame(conn, maxFrame, protocol.KindHelloAck, 0, map[string]any{
		"protocol":        protocol.Version,
		"server":          "envd " + Version,
		"max_frame":       maxFrame,
		"credits_initial": 65536,
		"resume":          []any{},
		"transport":       "tcp",
	}); err != nil {
		return
	}

	open, err := protocol.ReadFrame(conn, maxFrame)
	if err != nil || open.Kind != protocol.KindOpen {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 12, "name": "UNSUPPORTED", "message": "expected OPEN",
		})
		return
	}
	var openPayload struct {
		SessionID string `json:"session_id"`
		Cols      uint16 `json:"cols"`
		Rows      uint16 `json:"rows"`
	}
	if err := json.Unmarshal(open.Payload, &openPayload); err != nil || openPayload.SessionID == "" {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 1, "name": "INVALID_ARGUMENT", "message": "session_id is required",
		})
		return
	}
	session, err := s.getSession(openPayload.SessionID)
	if err != nil {
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 404, "name": "NOT_FOUND", "message": err.Error(),
		})
		return
	}

	session.mu.Lock()
	if session.closed {
		session.mu.Unlock()
		_ = writeServiceFrame(conn, maxFrame, protocol.KindError, 0, map[string]any{
			"code": 9, "name": "FAILED_PRECONDITION", "message": "session is closed",
		})
		return
	}
	pending := append([]byte(nil), session.pending...)
	session.pending = session.pending[:0]
	if openPayload.Cols > 0 && openPayload.Rows > 0 && session.pty != nil {
		_ = session.pty.Resize(int(openPayload.Cols), int(openPayload.Rows))
	}
	session.mu.Unlock()
	_ = conn.SetDeadline(time.Time{})

	var writeMu sync.Mutex
	writeFrame := func(kind protocol.Kind, payload []byte) error {
		writeMu.Lock()
		defer writeMu.Unlock()
		return protocol.WriteFrame(conn, protocol.Frame{Kind: kind, Payload: payload}, maxFrame)
	}
	writeJSON := func(kind protocol.Kind, value any) error {
		payload, err := json.Marshal(value)
		if err != nil {
			return err
		}
		return writeFrame(kind, payload)
	}
	if err := writeJSON(protocol.KindOpenAck, map[string]any{
		"session_id": openPayload.SessionID,
		"cols":       openPayload.Cols,
		"rows":       openPayload.Rows,
		"pending":    len(pending),
	}); err != nil {
		return
	}
	if len(pending) > 0 {
		if err := writeFrame(protocol.KindData, pending); err != nil {
			return
		}
	}

	clientErr := make(chan error, 1)
	go func() {
		for {
			frame, err := protocol.ReadFrame(conn, maxFrame)
			if err != nil {
				clientErr <- err
				return
			}
			switch frame.Kind {
			case protocol.KindData:
				session.mu.Lock()
				err := session.writeRaw(frame.Payload)
				session.mu.Unlock()
				if err != nil {
					clientErr <- err
					return
				}
			case protocol.KindResize:
				var resize struct {
					Cols uint16 `json:"cols"`
					Rows uint16 `json:"rows"`
				}
				if err := json.Unmarshal(frame.Payload, &resize); err != nil {
					clientErr <- err
					return
				}
				session.mu.Lock()
				if session.pty != nil && resize.Cols > 0 && resize.Rows > 0 {
					_ = session.pty.Resize(int(resize.Cols), int(resize.Rows))
				}
				session.mu.Unlock()
			case protocol.KindPing:
				if err := writeFrame(protocol.KindPong, nil); err != nil {
					clientErr <- err
					return
				}
			case protocol.KindCredit:
				// The service data plane currently sends immediately; credit
				// frames are accepted for protocol compatibility.
			case protocol.KindClose:
				clientErr <- errDataClosed
				return
			default:
				clientErr <- fmt.Errorf("unsupported frame %s", frame.Kind)
				return
			}
		}
	}()

	for {
		select {
		case chunk, ok := <-session.chunks:
			if !ok {
				_ = writeJSON(protocol.KindExit, map[string]any{"exit_code": 0})
				return
			}
			if err := writeFrame(protocol.KindData, chunk); err != nil {
				return
			}
		case err := <-clientErr:
			if errors.Is(err, errDataClosed) || errors.Is(err, io.EOF) {
				return
			}
			_ = writeJSON(protocol.KindError, map[string]any{
				"code": 13, "name": "INTERNAL", "message": err.Error(),
			})
			return
		}
	}
}

func writeServiceFrame(conn net.Conn, maxFrame int, kind protocol.Kind, streamID uint32, value any) error {
	payload, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return protocol.WriteFrame(conn, protocol.Frame{
		Kind:     kind,
		StreamID: streamID,
		Payload:  payload,
	}, maxFrame)
}
