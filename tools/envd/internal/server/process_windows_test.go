//go:build windows

package server

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"golang.org/x/sys/windows"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func readWindowsPID(t *testing.T, path string) int {
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

func waitForWindowsExit(t *testing.T, pid int) {
	t.Helper()
	handle, err := windows.OpenProcess(windows.SYNCHRONIZE|windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		// The process may already be gone, which is the desired outcome.
		return
	}
	defer windows.CloseHandle(handle)
	event, err := windows.WaitForSingleObject(handle, 5000)
	if err != nil {
		t.Fatalf("wait for process %d: %v", pid, err)
	}
	if event != windows.WAIT_OBJECT_0 {
		t.Fatalf("process %d survived the job-object teardown (wait event %#x)", pid, event)
	}
}

func TestExecTimeoutKillsJobTree(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"tree"}`)
	id := env["env_id"].(string)
	pidFile := filepath.Join(t.TempDir(), "pid")
	command := fmt.Sprintf(
		"$p = Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 30' -PassThru; Set-Content -Path '%s' -Value $p.Id; Start-Sleep -Seconds 30",
		strings.ReplaceAll(pidFile, "'", "''"),
	)
	args, _ := json.Marshal(map[string]any{
		"env_id":     id,
		"argv":       []string{"powershell.exe", "-NoProfile", "-Command", command},
		"timeout_ms": 8000,
	})
	detail := toolError(t, s.callTool("exec", json.RawMessage(args)))
	if detail["name"] != "DEADLINE_EXCEEDED" {
		t.Fatalf("error = %v", detail)
	}
	waitForWindowsExit(t, readWindowsPID(t, pidFile))
}
