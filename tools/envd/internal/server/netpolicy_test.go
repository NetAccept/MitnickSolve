package server

import (
	"encoding/json"
	"testing"
)

func TestNetAllowlistDeniesOutsideTargets(t *testing.T) {
	s := New(Options{Root: t.TempDir(), NetAllow: []string{"127.0.0.0/8,example.test"}})
	created := mustTool(t, s, "env_create", `{"name":"scope"}`)
	id := created["env_id"].(string)

	if err := s.authorizeNetHost("127.0.0.1"); err != nil {
		t.Fatalf("loopback should be allowed: %v", err)
	}
	if err := s.authorizeNetHost("api.example.test"); err != nil {
		t.Fatalf("domain suffix should be allowed: %v", err)
	}
	if err := s.authorizeNetHost("192.0.2.1"); err == nil {
		t.Fatal("TEST-NET address should be denied")
	}

	args, _ := json.Marshal(map[string]any{
		"env_id": id,
		"host":   "192.0.2.1",
		"port":   80,
	})
	detail := toolError(t, s.toolNetDial(args))
	if detail["name"] != "PERMISSION_DENIED" {
		t.Fatalf("net_dial outside allowlist = %v", detail)
	}

	args, _ = json.Marshal(map[string]any{
		"env_id": id,
		"url":    "http://198.51.100.1/",
	})
	detail = toolError(t, s.toolWebRequest(args))
	if detail["name"] != "PERMISSION_DENIED" {
		t.Fatalf("web_request outside allowlist = %v", detail)
	}
}

func TestNetAllowlistValidation(t *testing.T) {
	if err := ValidateNetAllow([]string{"10.0.0.0/8", "example.test"}); err != nil {
		t.Fatalf("valid allowlist rejected: %v", err)
	}
	if err := ValidateNetAllow([]string{"not a rule"}); err == nil {
		t.Fatal("invalid allowlist entry was accepted")
	}
}
