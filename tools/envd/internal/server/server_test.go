package server

import (
	"encoding/base64"
	"encoding/json"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func TestSecureJoin(t *testing.T) {
	root := t.TempDir()
	if _, err := secureJoin(root, "a/b"); err != nil {
		t.Fatalf("valid path rejected: %v", err)
	}
	bad := []string{"../x", "a/../../b", filepath.Join(root, "abs")}
	for _, rel := range bad {
		if _, err := secureJoin(root, rel); err == nil {
			t.Errorf("accepted escaping path %q", rel)
		}
	}
}

func TestEnvFileRoundTrip(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"t"}`)
	id, _ := env["env_id"].(string)
	if id == "" {
		t.Fatal("missing env_id")
	}

	payload := base64.StdEncoding.EncodeToString([]byte("hello"))
	write := mustTool(t, s, "write_file",
		`{"env_id":"`+id+`","path":"a.txt","data":"`+payload+`","atomic":true}`)
	if write["size"].(float64) != 5 {
		t.Fatalf("unexpected size: %v", write["size"])
	}

	read := mustTool(t, s, "read_file", `{"env_id":"`+id+`","path":"a.txt"}`)
	decoded, err := base64.StdEncoding.DecodeString(read["data"].(string))
	if err != nil {
		t.Fatal(err)
	}
	if string(decoded) != "hello" {
		t.Fatalf("got %q", decoded)
	}
}

func TestExec(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"t"}`)
	id := env["env_id"].(string)

	argv := []string{"/bin/sh", "-c", "echo exec-ok"}
	if runtime.GOOS == "windows" {
		argv = []string{"cmd.exe", "/C", "echo exec-ok"}
	}
	args, err := json.Marshal(map[string]any{
		"env_id":     id,
		"argv":       argv,
		"timeout_ms": 10000,
	})
	if err != nil {
		t.Fatal(err)
	}
	out := mustTool(t, s, "exec", string(args))
	if out["exit_code"].(float64) != 0 {
		t.Fatalf("exit code: %v", out["exit_code"])
	}
	if !strings.Contains(out["stdout"].(string), "exec-ok") {
		t.Fatalf("stdout: %q", out["stdout"])
	}
}

func TestDispatch(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})

	resp, ok := s.dispatch(rpcRequest{
		JSONRPC: "2.0",
		ID:      json.RawMessage("1"),
		Method:  "initialize",
	})
	if !ok {
		t.Fatal("initialize produced no response")
	}
	result, ok := resp.Result.(map[string]any)
	if !ok || result["protocolVersion"] == nil {
		t.Fatalf("bad initialize result: %#v", resp.Result)
	}

	resp, ok = s.dispatch(rpcRequest{
		JSONRPC: "2.0",
		ID:      json.RawMessage("2"),
		Method:  "tools/list",
	})
	if !ok {
		t.Fatal("tools/list produced no response")
	}
	result = resp.Result.(map[string]any)
	tools, ok := result["tools"].([]map[string]any)
	if !ok || len(tools) == 0 {
		t.Fatalf("bad tools/list result: %#v", result)
	}
}

func TestSessionExec(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"t"}`)
	id := env["env_id"].(string)

	sess := mustTool(t, s, "session_create", `{"env_id":"`+id+`"}`)
	sid := sess["session_id"].(string)
	if sid == "" {
		t.Fatal("missing session_id")
	}

	first := mustTool(t, s, "session_exec",
		`{"session_id":"`+sid+`","command":"echo one"}`)
	if !strings.Contains(first["stdout"].(string), "one") {
		t.Fatalf("first stdout: %q", first["stdout"])
	}
	if first["exit_code"].(float64) != 0 {
		t.Fatalf("first exit code: %v", first["exit_code"])
	}

	second := mustTool(t, s, "session_exec",
		`{"session_id":"`+sid+`","command":"echo two"}`)
	if !strings.Contains(second["stdout"].(string), "two") {
		t.Fatalf("second stdout: %q", second["stdout"])
	}

	if runtime.GOOS == "windows" {
		mustTool(t, s, "session_exec", `{"session_id":"`+sid+`","command":"set MSK_BENCH=42"}`)
		state := mustTool(t, s, "session_exec",
			`{"session_id":"`+sid+`","command":"echo %MSK_BENCH%"}`)
		if !strings.Contains(state["stdout"].(string), "42") {
			t.Fatalf("state not preserved: %q", state["stdout"])
		}
	} else {
		mustTool(t, s, "session_exec", `{"session_id":"`+sid+`","command":"MSK_BENCH=42"}`)
		state := mustTool(t, s, "session_exec",
			`{"session_id":"`+sid+`","command":"echo $MSK_BENCH"}`)
		if !strings.Contains(state["stdout"].(string), "42") {
			t.Fatalf("state not preserved: %q", state["stdout"])
		}
	}

	failCommand := "false"
	if runtime.GOOS == "windows" {
		failCommand = "cmd /c exit 1"
	}
	failed := mustTool(t, s, "session_exec",
		`{"session_id":"`+sid+`","command":"`+failCommand+`"}`)
	if failed["exit_code"].(float64) != 1 {
		t.Fatalf("fail exit code: %v", failed["exit_code"])
	}

	mustTool(t, s, "session_close", `{"session_id":"`+sid+`"}`)
}

func mustTool(t *testing.T, s *Server, name, args string) map[string]any {
	t.Helper()
	result := s.callTool(name, json.RawMessage(args))
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
	if envelope.IsError {
		t.Fatalf("tool %s failed: %v", name, envelope.StructuredContent)
	}
	if envelope.StructuredContent["ok"] != true {
		t.Fatalf("tool %s not ok: %v", name, envelope.StructuredContent)
	}
	data, ok := envelope.StructuredContent["data"].(map[string]any)
	if !ok {
		t.Fatalf("tool %s data type: %T", name, envelope.StructuredContent["data"])
	}
	return data
}
