//go:build !windows

package server

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func readPID(t *testing.T, path string) int {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		data, err := os.ReadFile(path)
		if err == nil {
			pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
			if err == nil && pid > 0 {
				return pid
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("pid file %s was not written", path)
	return 0
}

func waitForExit(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		err := syscall.Kill(pid, 0)
		if errors.Is(err, syscall.ESRCH) {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("process %d survived process-tree teardown", pid)
}

func TestExecTimeoutKillsProcessGroup(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"tree"}`)
	id := env["env_id"].(string)
	pidFile := filepath.Join(t.TempDir(), "pid")
	command := fmt.Sprintf("sleep 30 & echo $! > %s; wait", pidFile)
	args, _ := json.Marshal(map[string]any{
		"env_id":     id,
		"argv":       []string{"/bin/sh", "-c", command},
		"timeout_ms": 300,
	})
	detail := toolError(t, s.callTool("exec", json.RawMessage(args)))
	if detail["name"] != "DEADLINE_EXCEEDED" {
		t.Fatalf("error = %v", detail)
	}
	waitForExit(t, readPID(t, pidFile))
}

func TestSessionCloseKillsProcessTree(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"tree"}`)
	id := env["env_id"].(string)
	sess := mustTool(t, s, "session_create", `{"env_id":"`+id+`","pty":false}`)
	sessionID := sess["session_id"].(string)
	pidFile := filepath.Join(t.TempDir(), "pid")
	command := fmt.Sprintf("sleep 30 >/dev/null 2>&1 & echo $! > %s", pidFile)
	payload, _ := json.Marshal(map[string]any{
		"session_id": sessionID,
		"command":    command,
	})
	mustTool(t, s, "session_exec", string(payload))
	pid := readPID(t, pidFile)
	mustTool(t, s, "session_close", `{"session_id":"`+sessionID+`"}`)
	waitForExit(t, pid)
}
