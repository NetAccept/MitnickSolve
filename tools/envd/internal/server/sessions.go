package server

import (
	"bytes"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	gopty "github.com/aymanbagabas/go-pty"
)

const sessionPromptMarker = "__MSK_PROMPT__"

// Session is a persistent shell inside an environment. Pipe-backed sessions
// support marker-based session_exec; PTY-backed sessions support raw
// session_write/session_read/resize for interactive programs.
type Session struct {
	ID    string
	EnvID string

	mu        sync.Mutex
	cmd       *exec.Cmd
	ptyCmd    *gopty.Cmd
	pty       gopty.Pty
	stdin     io.WriteCloser
	output    *os.File
	chunks    chan []byte
	pending   []byte
	markerSeq uint64
	closed    bool
	replay    *outputLog
	// dataViews counts attached binary data-plane views. While nonzero,
	// readLoop mirrors output into the log but does not feed the control-plane
	// chunks, so a data view owns the stream. Do not mix session_read with a
	// data view.
	dataViews atomic.Int32
}

func (s *Server) toolSessionCreate(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Shell string `json:"shell"`
		Pty   bool   `json:"pty"`
		Cols  uint16 `json:"cols"`
		Rows  uint16 `json:"rows"`
		// OutputBufferBytes is the replay log capacity (64 KiB..64 MiB).
		OutputBufferBytes int `json:"output_buffer_bytes"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}

	shell, shellArgs, err := resolveShell(args.Shell)
	if err != nil {
		return errResult(codeUnsupported, "UNSUPPORTED", err.Error())
	}
	shellPath := shell
	if env.Profile != "isolated" {
		if resolved, err := exec.LookPath(shell); err == nil {
			shellPath = resolved
		}
	}
	launch, err := s.launch(env, append([]string{shellPath}, shellArgs...))
	if err != nil {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", err.Error())
	}

	cols := args.Cols
	if cols == 0 {
		cols = 120
	}
	rows := args.Rows
	if rows == 0 {
		rows = 40
	}

	capacity := args.OutputBufferBytes
	if capacity <= 0 {
		capacity = 4 << 20
	}
	if capacity < 64<<10 {
		capacity = 64 << 10
	}
	if capacity > 64<<20 {
		capacity = 64 << 20
	}

	sess := &Session{
		ID:     newID("ses"),
		EnvID:  env.ID,
		chunks: make(chan []byte, 256),
		replay: newOutputLog(capacity),
	}

	if args.Pty {
		p, err := gopty.New()
		if err != nil {
			return errResult(codeUnsupported, "UNSUPPORTED", "pty unavailable: "+err.Error())
		}
		cmd := p.Command(launch[0], launch[1:]...)
		cmd.Dir = env.Workspace
		if runtime.GOOS == "windows" {
			cmd.Env = launchEnv("TERM=xterm-256color", "PROMPT="+sessionPromptMarker)
		} else {
			cmd.Env = launchEnv("TERM=xterm-256color", "PS1="+sessionPromptMarker)
		}
		if err := cmd.Start(); err != nil {
			_ = p.Close()
			return sessionStartError(env, err)
		}
		_ = p.Resize(int(cols), int(rows))
		sess.pty = p
		sess.ptyCmd = cmd
		go sess.readLoop(p)
		go func() { _ = cmd.Wait() }()
	} else {
		cmd := exec.Command(launch[0], launch[1:]...)
		cmd.Dir = env.Workspace
		if runtime.GOOS == "windows" {
			cmd.Env = launchEnv("PROMPT=" + sessionPromptMarker)
		} else {
			cmd.Env = launchEnv()
		}
		stdin, err := cmd.StdinPipe()
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		output, writer, err := os.Pipe()
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		cmd.Stdout = writer
		cmd.Stderr = writer
		if err := cmd.Start(); err != nil {
			_ = writer.Close()
			_ = output.Close()
			_ = stdin.Close()
			return sessionStartError(env, err)
		}
		_ = writer.Close()
		sess.cmd = cmd
		sess.stdin = stdin
		sess.output = output
		go sess.readLoop(output)
		go func() { _ = cmd.Wait() }()
	}

	readyMarker := "__MSK_READY_" + randomToken() + "__"
	if err := sess.sendReady(readyMarker); err != nil {
		sess.close()
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	if err := sess.waitReady(readyMarker, 5*time.Second); err != nil {
		sess.close()
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	s.mu.Lock()
	s.sessions[sess.ID] = sess
	s.mu.Unlock()

	return okResult("created "+sess.ID, map[string]any{
		"session_id": sess.ID,
		"state":      "running",
		"shell":      shell,
		"profile":    env.Profile,
		"isolation":  env.Isolation,
		"pty":        args.Pty,
		"cols":       cols,
		"rows":       rows,
	})
}

func (s *Server) toolSessionExec(raw json.RawMessage) toolResult {
	var args struct {
		SessionID      string `json:"session_id"`
		Command        string `json:"command"`
		TimeoutMS      int64  `json:"timeout_ms"`
		MaxOutputBytes int64  `json:"max_output_bytes"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Command == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "command is required")
	}
	if args.TimeoutMS <= 0 {
		args.TimeoutMS = 30_000
	}
	if args.MaxOutputBytes <= 0 || args.MaxOutputBytes > 8<<20 {
		args.MaxOutputBytes = 1 << 20
	}

	sess, err := s.getSession(args.SessionID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}

	sess.mu.Lock()
	defer sess.mu.Unlock()
	if sess.closed {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", "session is closed")
	}
	if sess.pty != nil {
		return errResult(codeUnsupported, "UNSUPPORTED",
			"PTY sessions use session_write/session_read/resize, not session_exec")
	}
	if sess.dataViews.Load() > 0 {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
			"session has an attached data view; use the data plane")
	}

	marker := fmt.Sprintf("__MSK_DONE_%d_%s__", sess.markerSeq, randomToken())
	sess.markerSeq++
	script := sessionScript(args.Command, marker)
	sess.pending = sess.pending[:0]
	if err := sess.writeRaw([]byte(script)); err != nil {
		sess.closeLocked()
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	started := time.Now()
	deadline := time.NewTimer(time.Duration(args.TimeoutMS) * time.Millisecond)
	defer deadline.Stop()

	var out bytes.Buffer
	truncated := false
	pending := sess.pending
	for {
		for {
			index := bytes.IndexByte(pending, '\n')
			if index < 0 {
				break
			}
			line := string(pending[:index+1])
			pending = pending[index+1:]
			done, code := handleSessionLine(line, marker, &out, args.MaxOutputBytes, &truncated)
			if done {
				sess.pending = pending
				return okResult(fmt.Sprintf("exit code %d", code), map[string]any{
					"stdout":      out.String(),
					"exit_code":   code,
					"truncated":   truncated,
					"duration_ms": time.Since(started).Milliseconds(),
				})
			}
		}
		sess.pending = pending

		select {
		case chunk, ok := <-sess.chunks:
			if !ok {
				sess.closed = true
				return errResult(codeInternal, "INTERNAL",
					"session exited before marker; partial output: "+out.String())
			}
			pending = append(pending, chunk...)
		case <-deadline.C:
			sess.closeLocked()
			return errResult(codeDeadlineExceeded, "DEADLINE_EXCEEDED",
				fmt.Sprintf("command exceeded %d ms", args.TimeoutMS))
		}
	}
}

func (s *Server) toolSessionWrite(raw json.RawMessage) toolResult {
	var args struct {
		SessionID  string `json:"session_id"`
		DataBase64 string `json:"data_base64"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	data, err := base64.StdEncoding.DecodeString(args.DataBase64)
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "data_base64: "+err.Error())
	}
	if len(data) > 1<<20 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "write exceeds 1 MiB")
	}

	sess, err := s.getSession(args.SessionID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	sess.mu.Lock()
	defer sess.mu.Unlock()
	if sess.closed {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", "session is closed")
	}
	if err := sess.writeRaw(data); err != nil {
		sess.closeLocked()
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	return okResult(fmt.Sprintf("wrote %d bytes", len(data)), map[string]any{
		"bytes": len(data),
	})
}

func (s *Server) toolSessionRead(raw json.RawMessage) toolResult {
	var args struct {
		SessionID string `json:"session_id"`
		MaxBytes  int64  `json:"max_bytes"`
		TimeoutMS int64  `json:"timeout_ms"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	maxBytes := args.MaxBytes
	if maxBytes <= 0 || maxBytes > 1<<20 {
		maxBytes = 64 << 10
	}

	sess, err := s.getSession(args.SessionID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	sess.mu.Lock()
	defer sess.mu.Unlock()
	if sess.closed {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", "session is closed")
	}
	if sess.dataViews.Load() > 0 {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
			"session has an attached data view; read from the data plane")
	}

	eof := false
	if args.TimeoutMS <= 0 {
	drain:
		for {
			select {
			case chunk, ok := <-sess.chunks:
				if !ok {
					eof = true
					break drain
				}
				sess.pending = append(sess.pending, chunk...)
			default:
				break drain
			}
		}
	} else {
		timer := time.NewTimer(time.Duration(args.TimeoutMS) * time.Millisecond)
		defer timer.Stop()
	wait:
		for int64(len(sess.pending)) < maxBytes {
			select {
			case chunk, ok := <-sess.chunks:
				if !ok {
					eof = true
					break wait
				}
				sess.pending = append(sess.pending, chunk...)
			case <-timer.C:
				break wait
			}
		}
	}

	n := int64(len(sess.pending))
	if n > maxBytes {
		n = maxBytes
	}
	data := sess.pending[:n]
	sess.pending = sess.pending[n:]
	if eof {
		sess.closed = true
	}
	return okResult(fmt.Sprintf("read %d bytes", len(data)), map[string]any{
		"data_base64": base64.StdEncoding.EncodeToString(data),
		"bytes":       len(data),
		"remaining":   len(sess.pending),
		"eof":         eof,
	})
}

func (s *Server) toolSessionResize(raw json.RawMessage) toolResult {
	var args struct {
		SessionID string `json:"session_id"`
		Cols      uint16 `json:"cols"`
		Rows      uint16 `json:"rows"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Cols == 0 || args.Rows == 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "cols and rows are required")
	}
	sess, err := s.getSession(args.SessionID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	sess.mu.Lock()
	defer sess.mu.Unlock()
	if sess.closed {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", "session is closed")
	}
	if sess.pty == nil {
		return errResult(codeUnsupported, "UNSUPPORTED", "session is not a PTY")
	}
	if err := sess.pty.Resize(int(args.Cols), int(args.Rows)); err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	return okResult("resized", map[string]any{
		"cols": args.Cols,
		"rows": args.Rows,
	})
}

func (s *Server) toolSessionList(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
	}
	_ = json.Unmarshal(raw, &args)

	s.mu.Lock()
	defer s.mu.Unlock()
	sessions := make([]map[string]any, 0, len(s.sessions))
	for _, sess := range s.sessions {
		if args.EnvID != "" && sess.EnvID != args.EnvID {
			continue
		}
		sessions = append(sessions, map[string]any{
			"session_id": sess.ID,
			"env_id":     sess.EnvID,
			"pty":        sess.pty != nil,
		})
	}
	return okResult(fmt.Sprintf("%d sessions", len(sessions)), map[string]any{
		"sessions": sessions,
	})
}

func (s *Server) toolSessionClose(raw json.RawMessage) toolResult {
	var args struct {
		SessionID string `json:"session_id"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}

	s.mu.Lock()
	sess, ok := s.sessions[args.SessionID]
	if ok {
		delete(s.sessions, args.SessionID)
	}
	s.mu.Unlock()
	if !ok {
		return errResult(codeNotFound, "NOT_FOUND", "session not found: "+args.SessionID)
	}
	sess.close()
	return okResult("closed "+args.SessionID, map[string]any{
		"session_id": args.SessionID,
		"closed":     true,
	})
}

func (s *Server) getSession(id string) (*Session, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	sess, ok := s.sessions[id]
	if !ok {
		return nil, fmt.Errorf("session not found: %s", id)
	}
	return sess, nil
}

func (sess *Session) readLoop(reader io.Reader) {
	defer func() {
		close(sess.chunks)
		sess.replay.finish()
	}()
	buffer := make([]byte, 4096)
	for {
		n, err := reader.Read(buffer)
		if n > 0 {
			chunk := make([]byte, n)
			copy(chunk, buffer[:n])
			sess.replay.append(chunk)
			if sess.dataViews.Load() == 0 {
				sess.chunks <- chunk
			}
		}
		if err != nil {
			return
		}
	}
}

func (sess *Session) sendReady(marker string) error {
	var script string
	if runtime.GOOS == "windows" {
		script = "echo " + marker + "\r\n"
	} else {
		script = "printf '" + marker + "\\n'\n"
	}
	return sess.writeRaw([]byte(script))
}

func (sess *Session) writeRaw(data []byte) error {
	if sess.pty != nil {
		_, err := sess.pty.Write(data)
		return err
	}
	_, err := sess.stdin.Write(data)
	return err
}

func (sess *Session) waitReady(marker string, timeout time.Duration) error {
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()
	var pending []byte
	for {
		select {
		case chunk, ok := <-sess.chunks:
			if !ok {
				return fmt.Errorf("shell exited before ready")
			}
			pending = append(pending, chunk...)
			if bytes.Contains(pending, []byte(marker)) {
				return nil
			}
			if len(pending) > 1<<20 {
				pending = pending[len(pending)-(1<<20):]
			}
		case <-deadline.C:
			return fmt.Errorf("shell did not become ready within %s", timeout)
		}
	}
}

func handleSessionLine(
	line, marker string,
	out *bytes.Buffer,
	limit int64,
	truncated *bool,
) (bool, int) {
	if index := strings.Index(line, marker); index >= 0 {
		prefix := cleanSessionLine(line[:index])
		if prefix != "" {
			appendLimited(out, prefix+"\n", limit, truncated)
		}
		return true, parseExitCode(line[index+len(marker):])
	}
	cleaned := cleanSessionLine(line)
	if cleaned != "" {
		appendLimited(out, cleaned+"\n", limit, truncated)
	}
	return false, 0
}

func cleanSessionLine(line string) string {
	line = strings.TrimSuffix(line, "\n")
	line = strings.TrimSuffix(line, "\r")
	line = strings.TrimPrefix(line, sessionPromptMarker)
	return strings.TrimRight(line, " \t")
}

func (sess *Session) close() {
	sess.mu.Lock()
	defer sess.mu.Unlock()
	sess.closeLocked()
}

func (sess *Session) closeLocked() {
	if sess.closed {
		return
	}
	sess.closed = true
	sess.replay.finish()
	if sess.pty != nil {
		_ = sess.pty.Close()
	}
	if sess.ptyCmd != nil && sess.ptyCmd.Process != nil {
		_ = sess.ptyCmd.Process.Kill()
	}
	if sess.cmd != nil && sess.cmd.Process != nil {
		_ = sess.cmd.Process.Kill()
	}
	if sess.stdin != nil {
		_ = sess.stdin.Close()
	}
	if sess.output != nil {
		_ = sess.output.Close()
	}
}

func sessionStartError(env *Env, err error) toolResult {
	if env.Profile == "isolated" {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
			"sandbox launcher failed to start")
	}
	return errResult(codeInternal, "INTERNAL", err.Error())
}

func resolveShell(requested string) (string, []string, error) {
	if runtime.GOOS == "windows" {
		switch requested {
		case "", "cmd":
			return "cmd.exe", []string{"/Q", "/V:ON", "/K"}, nil
		default:
			return "", nil, fmt.Errorf("shell %q is not supported on Windows in M0", requested)
		}
	}
	switch requested {
	case "", "sh":
		return "/bin/sh", nil, nil
	case "bash":
		return "/bin/bash", nil, nil
	default:
		return "", nil, fmt.Errorf("shell %q is not supported in M0", requested)
	}
}

func sessionScript(command, marker string) string {
	if runtime.GOOS == "windows" {
		return fmt.Sprintf("( %s ) 2>&1 & echo %s !ERRORLEVEL!\r\n", command, marker)
	}
	return fmt.Sprintf("{ %s\n} 2>&1\nprintf '\\n%s %%s\\n' \"$?\"\n", command, marker)
}

func appendLimited(buf *bytes.Buffer, value string, limit int64, truncated *bool) {
	if int64(buf.Len()) >= limit {
		*truncated = true
		return
	}
	remaining := limit - int64(buf.Len())
	if int64(len(value)) > remaining {
		buf.WriteString(value[:remaining])
		*truncated = true
		return
	}
	buf.WriteString(value)
}

func parseExitCode(value string) int {
	value = strings.TrimSpace(value)
	if value == "" {
		return -1
	}
	code, err := strconv.Atoi(value)
	if err != nil {
		return -1
	}
	return code
}

func randomToken() string {
	var buf [4]byte
	if _, err := rand.Read(buf[:]); err != nil {
		return strconv.FormatInt(time.Now().UnixNano(), 16)
	}
	return hex.EncodeToString(buf[:])
}
