//go:build windows

package server

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

func workspaceJunction(t *testing.T, workspace, name, target string) string {
	t.Helper()
	junction := filepath.Join(workspace, name)
	command := exec.Command("cmd", "/c", "mklink", "/J", junction, target)
	if output, err := command.CombinedOutput(); err != nil {
		t.Skipf("cannot create junction: %v (%s)", err, output)
	}
	return junction
}

func TestWorkspaceJunctionEscapeIsRejected(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	outside := t.TempDir()
	if err := os.WriteFile(filepath.Join(outside, "secret.txt"), []byte("outside"), 0o600); err != nil {
		t.Fatal(err)
	}
	workspaceJunction(t, workspace, "link", outside)

	detail := analysisError(t, s.toolFileHash, map[string]any{
		"env_id": envID,
		"path":   "link/secret.txt",
	})
	if detail["name"] != "PERMISSION_DENIED" && detail["name"] != "NOT_FOUND" {
		t.Fatalf("junction read was not rejected: %v", detail)
	}

	search := s.toolFileSearch([]byte(`{"env_id":"` + envID + `","root":"link","pattern":"outside"}`))
	if _, ok := decodeTool(t, search); ok {
		t.Fatalf("file_search followed a junction outside the workspace: %#v", search.StructuredContent)
	}
}

func TestArchiveExtractRejectsJunctionDestination(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	outside := t.TempDir()
	workspaceJunction(t, workspace, "link", outside)
	writeTestZip(t, filepath.Join(workspace, "bundle.zip"), []zipTestEntry{
		{name: "evil.txt", data: []byte("escaped")},
	})

	detail := analysisError(t, s.toolArchiveExtract, map[string]any{
		"env_id": envID,
		"path":   "bundle.zip",
		"dest":   "link/sub",
	})
	if detail["name"] != "PERMISSION_DENIED" {
		t.Fatalf("junction destination was not rejected: %v", detail)
	}
	if _, err := os.Stat(filepath.Join(outside, "sub", "evil.txt")); !os.IsNotExist(err) {
		t.Fatalf("archive escaped through a junction: %v", err)
	}
}
