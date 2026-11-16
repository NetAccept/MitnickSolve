package server

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func TestOversizedControlResponseBecomesRPCError(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: 1024})
	request := []byte(`{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"transform","arguments":{"op":"random_bytes","length":4096}}}`)
	var out bytes.Buffer
	if err := s.handleControl(&out, request); err != nil {
		t.Fatalf("handleControl returned %v (the transport must not die)", err)
	}
	frame, err := protocol.ReadFrame(&out, protocol.EmbeddedMaxFrame)
	if err != nil {
		t.Fatalf("read frame: %v", err)
	}
	var response struct {
		ID    json.RawMessage `json:"id"`
		Error *rpcError       `json:"error"`
	}
	if err := json.Unmarshal(frame.Payload, &response); err != nil {
		t.Fatalf("parse response: %v", err)
	}
	if string(response.ID) != "7" || response.Error == nil || response.Error.Code != -32005 {
		t.Fatalf("expected a frame-limit RPC error, got %s", frame.Payload)
	}
}

func TestSecretRedactionDoesNotRevealShortValues(t *testing.T) {
	if masked := redactSecret("abcdefghi"); strings.Contains(masked, "cdefg") {
		t.Fatalf("short secret leaked: %q", masked)
	}
	if masked := redactSecret("abcd"); masked != "****" {
		t.Fatalf("four-byte secret = %q", masked)
	}
	if masked := redactConnectionString("mysql://root:hunter2@db.example:3306/app"); strings.Contains(masked, "hunter2") {
		t.Fatalf("connection string leaked password: %q", masked)
	} else if !strings.Contains(masked, "db.example") {
		t.Fatalf("connection string lost host: %q", masked)
	}
}
