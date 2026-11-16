//go:build unix

package server

import (
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestFileHashRejectsFIFOWithoutBlocking(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	fifo := filepath.Join(workspace, "pipe")
	if err := syscall.Mkfifo(fifo, 0o600); err != nil {
		t.Skipf("mkfifo: %v", err)
	}
	done := make(chan map[string]any, 1)
	go func() {
		done <- analysisError(t, s.toolFileHash, map[string]any{
			"env_id": envID,
			"path":   "pipe",
		})
	}()
	select {
	case detail := <-done:
		if detail["name"] != "INVALID_ARGUMENT" {
			t.Fatalf("FIFO was not rejected: %v", detail)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("file_hash blocked on a FIFO")
	}
}
