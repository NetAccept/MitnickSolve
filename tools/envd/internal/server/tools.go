package server

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

const (
	codeInvalidArgument    = 1
	codePermissionDenied   = 3
	codeNotFound           = 4
	codeFailedPrecondition = 6
	codeResourceExhausted  = 7
	codeDeadlineExceeded   = 8
	codeInternal           = 11
	codeUnsupported        = 12
)

type toolContent struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

type toolResult struct {
	Content           []toolContent `json:"content"`
	StructuredContent any           `json:"structuredContent"`
	IsError           bool          `json:"isError,omitempty"`
}

func okResult(summary string, data any) toolResult {
	return toolResult{
		Content: []toolContent{{Type: "text", Text: summary}},
		StructuredContent: map[string]any{
			"ok":   true,
			"data": data,
		},
	}
}

func errResult(code int, name, message string) toolResult {
	return toolResult{
		Content: []toolContent{{Type: "text", Text: name + ": " + message}},
		StructuredContent: map[string]any{
			"ok": false,
			"error": map[string]any{
				"code":    code,
				"name":    name,
				"message": message,
			},
		},
		IsError: true,
	}
}

func (s *Server) callTool(name string, raw json.RawMessage) toolResult {
	if len(raw) == 0 {
		raw = json.RawMessage(`{}`)
	}
	switch name {
	case "agent_info":
		return s.toolAgentInfo()
	case "env_create":
		return s.toolEnvCreate(raw)
	case "env_open":
		return s.toolEnvOpen(raw)
	case "exec":
		return s.toolExec(raw)
	case "read_file":
		return s.toolReadFile(raw)
	case "write_file":
		return s.toolWriteFile(raw)
	case "list_dir":
		return s.toolListDir(raw)
	case "stat_file":
		return s.toolStatFile(raw)
	case "session_create":
		return s.toolSessionCreate(raw)
	case "session_exec":
		return s.toolSessionExec(raw)
	case "session_write":
		return s.toolSessionWrite(raw)
	case "session_read":
		return s.toolSessionRead(raw)
	case "session_resize":
		return s.toolSessionResize(raw)
	case "session_list":
		return s.toolSessionList(raw)
	case "session_close":
		return s.toolSessionClose(raw)
	default:
		return errResult(codeUnsupported, "UNSUPPORTED", "unknown tool: "+name)
	}
}

func (s *Server) toolAgentInfo() toolResult {
	isolation := map[string]any{
		"mode":                "none",
		"launcher_configured": s.sandboxConfigured(),
	}
	if s.sandboxConfigured() {
		isolation["launcher"] = filepath.Base(s.opts.Sandbox[0])
	}
	if s.isolatedAvailable() {
		isolation["mode"] = "launcher"
	}
	return okResult("envd "+Version, map[string]any{
		"version":         Version,
		"default_profile": "dev",
		"profiles":        s.profiles(),
		"transport":       s.transport,
		"isolation":       isolation,
		"capabilities":    s.capabilities(),
		"max_frame":       s.opts.MaxFrame,
	})
}

func (s *Server) toolEnvCreate(raw json.RawMessage) toolResult {
	var args struct {
		Name    string `json:"name"`
		Kind    string `json:"kind"`
		Root    string `json:"root"`
		Profile string `json:"profile"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Kind == "" {
		args.Kind = "managed"
	}
	if args.Kind != "managed" {
		return errResult(codeUnsupported, "UNSUPPORTED", "only managed environments are implemented in M0")
	}
	switch args.Profile {
	case "":
		args.Profile = "dev"
	case "dev":
	case "host":
		if s.transport == "embedded-stdio" {
			return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
				"host profile requires service transport; embedded-stdio only supports dev")
		}
	case "isolated":
		if s.transport == "embedded-stdio" {
			return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
				"isolated profile requires service transport and --sandbox-command")
		}
		if !s.sandboxConfigured() {
			return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
				"isolated profile requires envd --sandbox-command/--sandbox-arg")
		}
		if err := ValidateSandbox(s.opts.Sandbox, s.opts.Root); err != nil {
			return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
				"sandbox launcher is unusable: "+err.Error())
		}
	default:
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "unknown profile: "+args.Profile)
	}
	if args.Name == "" {
		args.Name = "env"
	}

	id := newID("env")
	var workspace string
	if args.Root != "" {
		full, err := secureJoin(s.opts.Root, args.Root)
		if err != nil {
			return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
		}
		workspace = full
	} else {
		workspace = filepath.Join(s.opts.Root, id)
	}
	if err := os.MkdirAll(workspace, 0o700); err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	isolation := "none"
	if args.Profile == "isolated" {
		isolation = "launcher"
	}
	env := &Env{
		ID:        id,
		Name:      args.Name,
		Workspace: workspace,
		Profile:   args.Profile,
		Isolation: isolation,
	}
	s.mu.Lock()
	s.envs[id] = env
	s.mu.Unlock()
	return okResult("created "+id, env)
}

func execStartFailure(env *Env, err error) toolResult {
	var execErr *exec.Error
	if env.Profile == "isolated" &&
		(errors.As(err, &execErr) || errors.Is(err, fs.ErrNotExist) || errors.Is(err, fs.ErrPermission)) {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
			"sandbox launcher failed to start")
	}
	return errResult(codeInternal, "INTERNAL", err.Error())
}

func (s *Server) toolEnvOpen(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	return okResult("opened "+env.ID, env)
}

func (s *Server) toolExec(raw json.RawMessage) toolResult {
	var args struct {
		EnvID          string   `json:"env_id"`
		Argv           []string `json:"argv"`
		Cwd            string   `json:"cwd"`
		TimeoutMS      int64    `json:"timeout_ms"`
		MaxOutputBytes int64    `json:"max_output_bytes"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if len(args.Argv) == 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "argv is required")
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	if args.TimeoutMS <= 0 {
		args.TimeoutMS = 30_000
	}
	if args.MaxOutputBytes <= 0 || args.MaxOutputBytes > 8<<20 {
		args.MaxOutputBytes = 1 << 20
	}

	cwd := env.Workspace
	if args.Cwd != "" {
		full, err := secureJoin(env.Workspace, args.Cwd)
		if err != nil {
			return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
		}
		cwd = full
	}

	launch, err := s.launch(env, args.Argv)
	if err != nil {
		return errResult(codeFailedPrecondition, "FAILED_PRECONDITION", err.Error())
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(args.TimeoutMS)*time.Millisecond)
	defer cancel()
	guard, err := newProcessGuard()
	if err != nil {
		return errResult(codeInternal, "INTERNAL", "create process job: "+err.Error())
	}
	defer guard.close()
	cmd := exec.CommandContext(ctx, launch[0], launch[1:]...)
	configurePipeProcess(cmd)
	cmd.Cancel = func() error { return processCancel(cmd) }
	cmd.Dir = cwd
	cmd.Env = launchEnv()
	cmd.WaitDelay = 5 * time.Second
	stdout := &limitedBuffer{limit: args.MaxOutputBytes}
	stderr := &limitedBuffer{limit: args.MaxOutputBytes}
	cmd.Stdout = stdout
	cmd.Stderr = stderr

	started := time.Now()
	if err := cmd.Start(); err != nil {
		return execStartFailure(env, err)
	}
	if err := guard.assign(cmd.Process.Pid); err != nil {
		killProcess(cmd)
		return errResult(codeInternal, "INTERNAL", "assign process job: "+err.Error())
	}
	err = cmd.Wait()
	duration := time.Since(started).Milliseconds()
	if errors.Is(ctx.Err(), context.DeadlineExceeded) {
		return errResult(codeDeadlineExceeded, "DEADLINE_EXCEEDED",
			fmt.Sprintf("command exceeded %d ms", args.TimeoutMS))
	}

	exitCode := 0
	if err != nil {
		var exitErr *exec.ExitError
		var execErr *exec.Error
		switch {
		case errors.As(err, &exitErr):
			exitCode = exitErr.ExitCode()
		case env.Profile == "isolated" &&
			(errors.As(err, &execErr) || errors.Is(err, fs.ErrNotExist) || errors.Is(err, fs.ErrPermission)):
			return errResult(codeFailedPrecondition, "FAILED_PRECONDITION",
				"sandbox launcher failed to start")
		default:
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
	}

	return okResult(fmt.Sprintf("exit code %d", exitCode), map[string]any{
		"exit_code":   exitCode,
		"signal":      0,
		"stdout":      stdout.String(),
		"stderr":      stderr.String(),
		"truncated":   stdout.truncated || stderr.truncated,
		"duration_ms": duration,
	})
}

func (s *Server) toolReadFile(raw json.RawMessage) toolResult {
	var args struct {
		EnvID  string `json:"env_id"`
		Path   string `json:"path"`
		Offset int64  `json:"offset"`
		Limit  int64  `json:"limit"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Limit <= 0 || args.Limit > 1<<20 {
		args.Limit = 1 << 20
	}
	if args.Offset < 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "offset must be non-negative")
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	full, err := secureJoin(env.Workspace, args.Path)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}

	file, err := os.Open(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	if info.IsDir() {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is a directory")
	}
	if args.Offset > 0 {
		if _, err := file.Seek(args.Offset, io.SeekStart); err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
	}

	buf := make([]byte, args.Limit)
	n, readErr := io.ReadFull(file, buf)
	if readErr != nil && !errors.Is(readErr, io.EOF) && !errors.Is(readErr, io.ErrUnexpectedEOF) {
		return errResult(codeInternal, "INTERNAL", readErr.Error())
	}
	buf = buf[:n]
	sum := sha256.Sum256(buf)
	next := args.Offset + int64(n)

	return okResult(fmt.Sprintf("read %d bytes", n), map[string]any{
		"data":        base64.StdEncoding.EncodeToString(buf),
		"next_offset": next,
		"eof":         next >= info.Size(),
		"size":        info.Size(),
		"sha256":      hex.EncodeToString(sum[:]),
	})
}

func (s *Server) toolWriteFile(raw json.RawMessage) toolResult {
	var args struct {
		EnvID  string `json:"env_id"`
		Path   string `json:"path"`
		Data   string `json:"data"`
		Atomic bool   `json:"atomic"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	data, err := base64.StdEncoding.DecodeString(args.Data)
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "data must be base64: "+err.Error())
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	full, err := secureJoin(env.Workspace, args.Path)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if err := os.MkdirAll(filepath.Dir(full), 0o700); err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	if args.Atomic {
		if err := writeAtomic(full, data); err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
	} else if err := os.WriteFile(full, data, 0o600); err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	sum := sha256.Sum256(data)
	return okResult(fmt.Sprintf("wrote %d bytes", len(data)), map[string]any{
		"size":   len(data),
		"sha256": hex.EncodeToString(sum[:]),
	})
}

func (s *Server) toolListDir(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	full, err := secureJoin(env.Workspace, args.Path)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	entries, err := os.ReadDir(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}

	const maxEntries = 5000
	truncated := false
	if len(entries) > maxEntries {
		entries = entries[:maxEntries]
		truncated = true
	}
	out := make([]map[string]any, 0, len(entries))
	for _, entry := range entries {
		size := int64(0)
		mode := ""
		if info, err := entry.Info(); err == nil {
			size = info.Size()
			mode = info.Mode().String()
		}
		out = append(out, map[string]any{
			"name":   entry.Name(),
			"is_dir": entry.IsDir(),
			"size":   size,
			"mode":   mode,
		})
	}
	return okResult(fmt.Sprintf("%d entries", len(out)), map[string]any{
		"entries":   out,
		"truncated": truncated,
	})
}

func (s *Server) toolStatFile(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	full, err := secureJoin(env.Workspace, args.Path)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	info, err := os.Lstat(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	return okResult("stat "+args.Path, map[string]any{
		"name":       info.Name(),
		"size":       info.Size(),
		"mode":       info.Mode().String(),
		"is_dir":     info.IsDir(),
		"is_symlink": info.Mode()&os.ModeSymlink != 0,
		"mtime":      info.ModTime().UTC().Format(time.RFC3339),
	})
}

func (s *Server) getEnv(id string) (*Env, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	env, ok := s.envs[id]
	if !ok {
		return nil, fmt.Errorf("environment not found: %s", id)
	}
	return env, nil
}

func newID(prefix string) string {
	var buf [8]byte
	if _, err := rand.Read(buf[:]); err != nil {
		return fmt.Sprintf("%s_%d", prefix, time.Now().UnixNano())
	}
	return prefix + "_" + hex.EncodeToString(buf[:])
}

// secureJoin confines a relative path to root. M0 handles traversal and
// existing symlinks; os.Root, reserved Windows names, and ADS rejection follow
// with the session/data-plane milestone.
func secureJoin(root, rel string) (string, error) {
	if rel == "" {
		return "", fmt.Errorf("empty path")
	}
	rel = filepath.FromSlash(rel)
	if filepath.IsAbs(rel) {
		return "", fmt.Errorf("absolute paths are not allowed")
	}
	clean := filepath.Clean(rel)
	if clean == "." || clean == ".." || strings.HasPrefix(clean, ".."+string(os.PathSeparator)) {
		return "", fmt.Errorf("path escapes root")
	}

	rootAbs, err := filepath.Abs(root)
	if err != nil {
		return "", err
	}
	full := filepath.Join(rootAbs, clean)
	check, err := filepath.Rel(rootAbs, full)
	if err != nil || check == ".." || strings.HasPrefix(check, ".."+string(os.PathSeparator)) {
		return "", fmt.Errorf("path escapes root")
	}

	if resolved, err := filepath.EvalSymlinks(full); err == nil {
		rootResolved, rootErr := filepath.EvalSymlinks(rootAbs)
		if rootErr != nil {
			rootResolved = rootAbs
		}
		relResolved, relErr := filepath.Rel(rootResolved, resolved)
		if relErr != nil || relResolved == ".." || strings.HasPrefix(relResolved, ".."+string(os.PathSeparator)) {
			return "", fmt.Errorf("symlink escapes root")
		}
	} else if !os.IsNotExist(err) {
		return "", err
	}

	return full, nil
}

func writeAtomic(path string, data []byte) error {
	dir := filepath.Dir(path)
	tmp, err := os.CreateTemp(dir, ".msk-*")
	if err != nil {
		return err
	}
	name := tmp.Name()
	defer os.Remove(name)

	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return err
	}
	_ = tmp.Chmod(0o600)
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(name, path)
}

type limitedBuffer struct {
	buf       bytes.Buffer
	limit     int64
	truncated bool
}

func (b *limitedBuffer) Write(p []byte) (int, error) {
	if b.limit <= 0 {
		b.truncated = true
		return len(p), nil
	}
	remaining := b.limit - int64(b.buf.Len())
	if remaining <= 0 {
		b.truncated = true
		return len(p), nil
	}
	if int64(len(p)) > remaining {
		b.buf.Write(p[:remaining])
		b.truncated = true
		return len(p), nil
	}
	b.buf.Write(p)
	return len(p), nil
}

func (b *limitedBuffer) String() string {
	return b.buf.String()
}
