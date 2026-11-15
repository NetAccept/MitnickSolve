package server

import (
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

// The test binary is also the portable sandbox launcher. This avoids bwrap and
// shell scripts, and exercises the same argv/env path on Ubuntu and Windows.
func TestMain(m *testing.M) {
	if mode := os.Getenv("MSK_TEST_SANDBOX_HELPER"); mode != "" {
		os.Exit(runSandboxHelper(mode))
	}
	os.Exit(m.Run())
}

type helperRecord struct {
	Target []string          `json:"target"`
	Cwd    string            `json:"cwd"`
	Env    map[string]string `json:"env"`
}

func runSandboxHelper(mode string) int {
	separator := -1
	for index, arg := range os.Args {
		if arg == "--" {
			separator = index
			break
		}
	}
	if separator < 0 || separator == len(os.Args)-1 {
		return 2
	}
	target := os.Args[separator+1:]
	if logPath := os.Getenv("MSK_TEST_SANDBOX_LOG"); logPath != "" {
		record := helperRecord{Target: target, Env: map[string]string{}}
		record.Cwd, _ = os.Getwd()
		for _, entry := range os.Environ() {
			name, value, ok := strings.Cut(entry, "=")
			if ok && (strings.HasPrefix(name, "MSK_TEST_") || name == "ENVD_TOKEN") {
				record.Env[name] = value
			}
		}
		payload, _ := json.Marshal(record)
		file, err := os.OpenFile(logPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
		if err == nil {
			_, _ = file.Write(append(payload, '\n'))
			_ = file.Close()
		}
	}
	switch mode {
	case "refuse":
		return 97
	case "hang":
		time.Sleep(time.Hour)
		return 0
	}
	cmd := exec.Command(target[0], target[1:]...)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	cmd.Env = os.Environ()
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) {
			return exit.ExitCode()
		}
		return 1
	}
	return 0
}

func readHelperRecords(t *testing.T, path string) []helperRecord {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read launcher log: %v", err)
	}
	var records []helperRecord
	for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n") {
		if line == "" {
			continue
		}
		var record helperRecord
		if err := json.Unmarshal([]byte(line), &record); err != nil {
			t.Fatalf("parse launcher log: %v", err)
		}
		records = append(records, record)
	}
	return records
}

func waitForLauncher(t *testing.T, path string) []helperRecord {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if data, err := os.ReadFile(path); err == nil && len(data) > 0 {
			return readHelperRecords(t, path)
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("launcher log %s was not written", path)
	return nil
}

func toolError(t *testing.T, result toolResult) map[string]any {
	t.Helper()
	raw, err := json.Marshal(result)
	if err != nil {
		t.Fatal(err)
	}
	var envelope struct {
		StructuredContent map[string]any `json:"structuredContent"`
		IsError           bool           `json:"isError"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil {
		t.Fatal(err)
	}
	if !envelope.IsError {
		t.Fatalf("expected an error result: %s", raw)
	}
	detail, ok := envelope.StructuredContent["error"].(map[string]any)
	if !ok {
		t.Fatalf("error detail type: %T", envelope.StructuredContent["error"])
	}
	return detail
}

func sandboxServer(t *testing.T, mode string) (*Server, string) {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	logPath := filepath.Join(t.TempDir(), "launcher.jsonl")
	t.Setenv("MSK_TEST_SANDBOX_HELPER", mode)
	t.Setenv("MSK_TEST_SANDBOX_LOG", logPath)
	s := New(Options{
		Root:     t.TempDir(),
		MaxFrame: protocol.EmbeddedMaxFrame,
		Sandbox:  []string{executable, "--"},
	})
	s.transport = "service-http"
	return s, logPath
}

func TestEmbeddedOnlyAdvertisesDev(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	if got := strings.Join(s.profiles(), ","); got != "dev" {
		t.Fatalf("embedded profiles = %q", got)
	}
	detail := toolError(t, s.callTool("env_create", json.RawMessage(`{"profile":"host"}`)))
	if detail["name"] != "FAILED_PRECONDITION" {
		t.Fatalf("error = %v", detail)
	}
	detail = toolError(t, s.callTool("env_create", json.RawMessage(`{"profile":"isolated"}`)))
	if detail["name"] != "FAILED_PRECONDITION" {
		t.Fatalf("error = %v", detail)
	}
}

func TestServiceIsolatedFailsClosedWithoutLauncher(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	s.transport = "service-http"
	if got := strings.Join(s.profiles(), ","); got != "dev,host" {
		t.Fatalf("service profiles = %q", got)
	}
	detail := toolError(t, s.callTool("env_create", json.RawMessage(`{"profile":"isolated"}`)))
	if detail["name"] != "FAILED_PRECONDITION" {
		t.Fatalf("error = %v", detail)
	}
	if message, _ := detail["message"].(string); !strings.Contains(message, "--sandbox-command") {
		t.Fatalf("error does not explain the launcher requirement: %v", detail)
	}
	if len(s.envs) != 0 {
		t.Fatalf("isolated env_create created an unenforced environment")
	}
}

func TestServiceHostDoesNotNeedLauncher(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	s.transport = "service-http"
	env := mustTool(t, s, "env_create", `{"profile":"host"}`)
	if env["profile"] != "host" || env["isolation"] != "none" {
		t.Fatalf("env = %v", env)
	}
}

func TestIsolatedLaunchArgvIsExactAndFailClosed(t *testing.T) {
	env := &Env{Profile: "isolated"}
	argv, err := env.launchArgv([]string{"echo", "$HOME; rm -rf /"}, []string{"bwrap", "--"})
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"bwrap", "--", "echo", "$HOME; rm -rf /"}
	if len(argv) != len(want) {
		t.Fatalf("argv = %v", argv)
	}
	for index := range want {
		if argv[index] != want[index] {
			t.Fatalf("argv[%d] = %q, want %q", index, argv[index], want[index])
		}
	}
	if _, err := env.launchArgv([]string{"echo"}, nil); err == nil {
		t.Fatal("isolated environment launched without a sandbox")
	}
	if _, err := env.launchArgv([]string{"--unshare-all"}, []string{"bwrap"}); err == nil {
		t.Fatal("isolated argv[0] was allowed to look like a launcher option")
	}
	if _, err := env.launchArgv([]string{"echo\x00x"}, []string{"bwrap"}); err == nil {
		t.Fatal("NUL in target argv was accepted")
	}
	dev := &Env{Profile: "dev"}
	argv, err = dev.launchArgv([]string{"echo"}, []string{"bwrap"})
	if err != nil || len(argv) != 1 || argv[0] != "echo" {
		t.Fatalf("dev launch = %v, %v", argv, err)
	}
}

func TestValidateSandbox(t *testing.T) {
	if err := ValidateSandbox(nil, t.TempDir()); err != nil {
		t.Fatalf("empty sandbox: %v", err)
	}
	if err := ValidateSandbox([]string{""}, t.TempDir()); err == nil {
		t.Fatal("empty launcher command accepted")
	}
	if err := ValidateSandbox([]string{"definitely-not-envd-launcher"}, t.TempDir()); err == nil {
		t.Fatal("missing launcher accepted")
	}
	if err := ValidateSandbox([]string{t.TempDir()}, t.TempDir()); err == nil {
		t.Fatal("directory launcher accepted")
	}
	if err := ValidateSandbox([]string{"echo\x00"}, t.TempDir()); err == nil {
		t.Fatal("NUL in launcher accepted")
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	if err := ValidateSandbox([]string{executable, "--"}, t.TempDir()); err != nil {
		t.Fatalf("test executable rejected: %v", err)
	}
	root := t.TempDir()
	inside := filepath.Join(root, "launcher")
	if err := os.WriteFile(inside, []byte("#!/bin/sh\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	if runtime.GOOS == "windows" {
		// Windows would reject this script before the data-root check.
		return
	}
	if err := ValidateSandbox([]string{inside}, root); err == nil {
		t.Fatal("launcher inside the client-writable data root accepted")
	}
}

func TestToolDefinitionsMatchProfiles(t *testing.T) {
	for _, transport := range []string{"embedded-stdio", "service-http"} {
		s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
		s.transport = transport
		definitions := toolDefinitions(s.profiles())
		var encode map[string]any
		for _, definition := range definitions {
			if definition["name"] == "env_create" {
				encode = definition["inputSchema"].(map[string]any)
			}
		}
		if encode == nil {
			t.Fatal("env_create schema missing")
		}
		properties := encode["properties"].(map[string]any)
		enum := properties["profile"].(map[string]any)["enum"].([]string)
		if strings.Join(enum, ",") != strings.Join(s.profiles(), ",") {
			t.Fatalf("%s schema enum %v != profiles %v", transport, enum, s.profiles())
		}
	}
}

func TestSandboxedExec(t *testing.T) {
	s, logPath := sandboxServer(t, "launcher")
	env := mustTool(t, s, "env_create", `{"profile":"isolated"}`)
	if env["isolation"] != "launcher" {
		t.Fatalf("env = %v", env)
	}
	id := env["env_id"].(string)
	argv := []string{"/bin/sh", "-c", "printf 'sandbox-ok %s\\n' \"$MSK_TEST_FORWARD\""}
	if runtime.GOOS == "windows" {
		argv = []string{"cmd.exe", "/C", "echo sandbox-ok %MSK_TEST_FORWARD%"}
	}
	t.Setenv("MSK_TEST_FORWARD", "forwarded")
	t.Setenv("ENVD_TOKEN", "service-secret")
	args, _ := json.Marshal(map[string]any{
		"env_id":     id,
		"argv":       argv,
		"timeout_ms": 10000,
	})
	out := mustTool(t, s, "exec", string(args))
	if !strings.Contains(out["stdout"].(string), "sandbox-ok forwarded") {
		t.Fatalf("stdout = %q", out["stdout"])
	}
	records := waitForLauncher(t, logPath)
	last := records[len(records)-1]
	if len(last.Target) != len(argv) {
		t.Fatalf("target = %v", last.Target)
	}
	for index := range argv {
		if last.Target[index] != argv[index] {
			t.Fatalf("target[%d] = %q, want %q", index, last.Target[index], argv[index])
		}
	}
	if last.Env["ENVD_TOKEN"] != "" {
		t.Fatalf("launcher inherited ENVD_TOKEN: %v", last.Env)
	}
	if last.Env["MSK_TEST_FORWARD"] != "forwarded" {
		t.Fatalf("non-secret environment was not forwarded: %v", last.Env)
	}
	if resolved, err := filepath.EvalSymlinks(last.Cwd); err == nil {
		if workspace, err := filepath.EvalSymlinks(env["workspace"].(string)); err == nil && !strings.EqualFold(resolved, workspace) {
			t.Fatalf("launcher cwd = %q, workspace = %q", resolved, workspace)
		}
	}
}

func TestDevProfileDoesNotUseSandbox(t *testing.T) {
	s, logPath := sandboxServer(t, "launcher")
	env := mustTool(t, s, "env_create", `{"profile":"dev"}`)
	id := env["env_id"].(string)
	argv := []string{"/bin/sh", "-c", "echo dev-ok"}
	if runtime.GOOS == "windows" {
		argv = []string{"cmd.exe", "/C", "echo dev-ok"}
	}
	args, _ := json.Marshal(map[string]any{"env_id": id, "argv": argv})
	mustTool(t, s, "exec", string(args))
	if _, err := os.Stat(logPath); !os.IsNotExist(err) {
		t.Fatalf("dev execution went through the isolated launcher: %v", err)
	}
}

func TestSandboxedPipeSession(t *testing.T) {
	s, logPath := sandboxServer(t, "launcher")
	env := mustTool(t, s, "env_create", `{"profile":"isolated"}`)
	id := env["env_id"].(string)
	sess := mustTool(t, s, "session_create", `{"env_id":"`+id+`","pty":false}`)
	sessionID := sess["session_id"].(string)
	t.Cleanup(func() {
		_ = s.callTool("session_close", json.RawMessage(`{"session_id":"`+sessionID+`"}`))
	})
	waitForLauncher(t, logPath)
	result := mustTool(t, s, "session_exec",
		`{"session_id":"`+sessionID+`","command":"echo session-sandbox"}`)
	if output, _ := result["stdout"].(string); !strings.Contains(output, "session-sandbox") {
		t.Fatalf("output = %q", output)
	}
}

func TestSandboxedPtySession(t *testing.T) {
	s, logPath := sandboxServer(t, "launcher")
	env := mustTool(t, s, "env_create", `{"profile":"isolated"}`)
	id := env["env_id"].(string)
	payload := `{"env_id":"` + id + `","pty":true}`
	raw := s.callTool("session_create", json.RawMessage(payload))
	data, ok := decodeTool(t, raw)
	if !ok {
		if runtime.GOOS != "linux" {
			t.Skip("PTY is unavailable on this host")
		}
		t.Fatalf("PTY session failed: %+v", raw)
	}
	sessionID := data["session_id"].(string)
	t.Cleanup(func() {
		_ = s.callTool("session_close", json.RawMessage(`{"session_id":"`+sessionID+`"}`))
	})
	waitForLauncher(t, logPath)
}

func TestSandboxRefusalDoesNotFallBack(t *testing.T) {
	s, logPath := sandboxServer(t, "refuse")
	env := mustTool(t, s, "env_create", `{"profile":"isolated"}`)
	id := env["env_id"].(string)
	argv := []string{"/bin/sh", "-c", "echo should-not-run"}
	if runtime.GOOS == "windows" {
		argv = []string{"cmd.exe", "/C", "echo should-not-run"}
	}
	args, _ := json.Marshal(map[string]any{"env_id": id, "argv": argv})
	result := s.callTool("exec", json.RawMessage(args))
	var envelope struct {
		StructuredContent map[string]any `json:"structuredContent"`
		IsError           bool           `json:"isError"`
	}
	raw, _ := json.Marshal(result)
	if err := json.Unmarshal(raw, &envelope); err != nil {
		t.Fatal(err)
	}
	if envelope.IsError {
		t.Fatalf("launcher refusal was reported as a tool error: %s", raw)
	}
	data := envelope.StructuredContent["data"].(map[string]any)
	if data["exit_code"].(float64) != 97 {
		t.Fatalf("exit code = %v", data["exit_code"])
	}
	waitForLauncher(t, logPath)
}

func TestSandboxLauncherMissingFailsClosed(t *testing.T) {
	s := New(Options{
		Root:     t.TempDir(),
		MaxFrame: protocol.EmbeddedMaxFrame,
		Sandbox:  []string{"/definitely/missing/envd-launcher"},
	})
	s.transport = "service-http"
	detail := toolError(t, s.callTool("env_create", json.RawMessage(`{"profile":"isolated"}`)))
	if detail["name"] != "FAILED_PRECONDITION" {
		t.Fatalf("error = %v", detail)
	}
	env := &Env{Profile: "isolated", Workspace: t.TempDir()}
	if _, err := s.launch(env, []string{"echo"}); err == nil {
		t.Fatal("launch accepted an unusable launcher")
	}
}

func TestEnvdTokenIsNotInherited(t *testing.T) {
	for _, profile := range []string{"dev", "isolated"} {
		t.Run(profile, func(t *testing.T) {
			s, logPath := sandboxServer(t, "launcher")
			if profile == "dev" {
				logPath = filepath.Join(t.TempDir(), "dev.jsonl")
				t.Setenv("MSK_TEST_SANDBOX_LOG", logPath)
			}
			t.Setenv("ENVD_TOKEN", "service-secret")
			t.Setenv("MSK_TEST_FORWARD", "visible")
			env := mustTool(t, s, "env_create", `{"profile":"`+profile+`"}`)
			id := env["env_id"].(string)
			argv := []string{"/bin/sh", "-c", "printf '%s' \"$MSK_TEST_FORWARD\""}
			if runtime.GOOS == "windows" {
				argv = []string{"cmd.exe", "/C", "echo %MSK_TEST_FORWARD%"}
			}
			args, _ := json.Marshal(map[string]any{"env_id": id, "argv": argv})
			out := mustTool(t, s, "exec", string(args))
			if !strings.Contains(out["stdout"].(string), "visible") {
				t.Fatalf("stdout = %q", out["stdout"])
			}
			if profile == "isolated" {
				records := waitForLauncher(t, logPath)
				last := records[len(records)-1]
				if last.Env["ENVD_TOKEN"] != "" {
					t.Fatalf("ENVD_TOKEN reached the launcher: %v", last.Env)
				}
				if last.Env["MSK_TEST_FORWARD"] != "visible" {
					t.Fatalf("non-secret env missing: %v", last.Env)
				}
			}
		})
	}
}

func TestAgentInfoReportsEnforcement(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	s.transport = "service-http"
	info := mustTool(t, s, "agent_info", `{}`)
	if info["transport"] != "service-http" {
		t.Fatalf("transport = %v", info["transport"])
	}
	profileNames := make([]string, 0)
	for _, profile := range info["profiles"].([]any) {
		profileNames = append(profileNames, profile.(string))
	}
	if !strings.Contains(strings.Join(profileNames, ","), "dev") {
		t.Fatalf("profiles = %v", info["profiles"])
	}
	isolation := info["isolation"].(map[string]any)
	if isolation["mode"] != "none" || isolation["launcher_configured"] != false {
		t.Fatalf("isolation = %v", isolation)
	}

	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("MSK_TEST_SANDBOX_HELPER", "launcher")
	s.opts.Sandbox = []string{executable, "--"}
	info = mustTool(t, s, "agent_info", `{}`)
	isolation = info["isolation"].(map[string]any)
	if isolation["mode"] != "launcher" || isolation["launcher_configured"] != true {
		t.Fatalf("isolation = %v", isolation)
	}
	caps := info["capabilities"].([]any)
	found := false
	for _, capability := range caps {
		if capability == "sandbox.launcher" {
			found = true
		}
	}
	if !found {
		t.Fatalf("sandbox.launcher missing from %v", caps)
	}
}
