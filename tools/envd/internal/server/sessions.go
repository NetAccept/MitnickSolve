package server

import (
	"bufio"
	"bytes"
	"crypto/rand"
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
	"time"
)

// Session is a persistent shell inside an environment. Commands are wrapped
// with a unique marker so the server can return exactly one command's output
// without prompt detection.
type Session struct {
	ID    string
	EnvID string

	mu        sync.Mutex
	cmd       *exec.Cmd
	stdin     io.WriteCloser
	output    *os.File
	lines     chan string
	markerSeq uint64
	closed    bool
}

func (s *Server) toolSessionCreate(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Shell string `json:"shell"`
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

	cmd := exec.Command(shell, shellArgs...)
	cmd.Dir = env.Workspace
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
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	_ = writer.Close()

	sess := &Session{
		ID:     newID("ses"),
		EnvID:  env.ID,
		cmd:    cmd,
		stdin:  stdin,
		output: output,
		lines:  make(chan string, 256),
	}
	go sess.readLoop()
	go func() { _ = cmd.Wait() }()

	s.mu.Lock()
	s.sessions[sess.ID] = sess
	s.mu.Unlock()

	return okResult("created "+sess.ID, map[string]any{
		"session_id": sess.ID,
		"state":      "running",
		"shell":      shell,
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

	marker := fmt.Sprintf("__MSK_DONE_%d_%s__", sess.markerSeq, randomToken())
	sess.markerSeq++
	script := sessionScript(args.Command, marker)
	if _, err := io.WriteString(sess.stdin, script); err != nil {
		sess.closed = true
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	started := time.Now()
	deadline := time.NewTimer(time.Duration(args.TimeoutMS) * time.Millisecond)
	defer deadline.Stop()

	var out bytes.Buffer
	truncated := false
	for {
		select {
		case line, ok := <-sess.lines:
			if !ok {
				sess.closed = true
				return errResult(codeInternal, "INTERNAL",
					"session exited before marker; partial output: "+out.String())
			}
			if index := strings.Index(line, marker); index >= 0 {
				appendLimited(&out, line[:index], args.MaxOutputBytes, &truncated)
				code := parseExitCode(line[index+len(marker):])
				return okResult(fmt.Sprintf("exit code %d", code), map[string]any{
					"stdout":      out.String(),
					"exit_code":   code,
					"truncated":   truncated,
					"duration_ms": time.Since(started).Milliseconds(),
				})
			}
			appendLimited(&out, line, args.MaxOutputBytes, &truncated)
		case <-deadline.C:
			sess.closed = true
			_ = sess.cmd.Process.Kill()
			_ = sess.output.Close()
			return errResult(codeDeadlineExceeded, "DEADLINE_EXCEEDED",
				fmt.Sprintf("command exceeded %d ms", args.TimeoutMS))
		}
	}
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

func (sess *Session) readLoop() {
	reader := bufio.NewReader(sess.output)
	defer close(sess.lines)
	for {
		line, err := reader.ReadString('\n')
		if line != "" {
			sess.lines <- line
		}
		if err != nil {
			return
		}
	}
}

func (sess *Session) close() {
	sess.mu.Lock()
	defer sess.mu.Unlock()
	if sess.closed {
		return
	}
	sess.closed = true
	if sess.cmd.Process != nil {
		_ = sess.cmd.Process.Kill()
	}
	_ = sess.stdin.Close()
	_ = sess.output.Close()
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
