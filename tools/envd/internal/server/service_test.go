package server

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func TestServiceHTTPAuthAndInitialize(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	handler := serviceHandler{server: s, token: "secret"}

	request := httptest.NewRequest(http.MethodPost, "/mcp",
		strings.NewReader(`{"jsonrpc":"2.0","id":1,"method":"initialize"}`))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated status: %d", response.Code)
	}

	request = httptest.NewRequest(http.MethodPost, "/mcp",
		strings.NewReader(`{"jsonrpc":"2.0","id":1,"method":"initialize"}`))
	request.Header.Set("Authorization", "Bearer secret")
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("authenticated status: %d body=%s", response.Code, response.Body.String())
	}
	var body struct {
		Result map[string]any `json:"result"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	serverInfo, _ := body.Result["serverInfo"].(map[string]any)
	if serverInfo["name"] != "envd" {
		t.Fatalf("serverInfo: %#v", body.Result)
	}

	health := httptest.NewRecorder()
	handler.ServeHTTP(health, httptest.NewRequest(http.MethodGet, "/healthz", nil))
	if health.Code != http.StatusOK {
		t.Fatalf("health status: %d", health.Code)
	}
	if !strings.Contains(health.Body.String(), `"ok":true`) {
		t.Fatalf("health body: %s", health.Body.String())
	}

	call := httptest.NewRequest(http.MethodPost, "/mcp", strings.NewReader(
		`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"env_create","arguments":{"name":"svc"}}}`))
	call.Header.Set("Authorization", "Bearer secret")
	callResponse := httptest.NewRecorder()
	handler.ServeHTTP(callResponse, call)
	if callResponse.Code != http.StatusOK {
		t.Fatalf("tools/call status: %d body=%s", callResponse.Code, callResponse.Body.String())
	}
	var callBody struct {
		Result struct {
			Structured map[string]any `json:"structuredContent"`
		} `json:"result"`
	}
	if err := json.Unmarshal(callResponse.Body.Bytes(), &callBody); err != nil {
		t.Fatal(err)
	}
	data, _ := callBody.Result.Structured["data"].(map[string]any)
	if data["env_id"] == "" {
		t.Fatalf("tools/call result: %#v", callBody.Result)
	}
}

func TestServiceDataHandshakeAndPTY(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"t"}`)
	envID := env["env_id"].(string)
	created := mustTool(t, s, "session_create",
		fmt.Sprintf(`{"env_id":%q,"pty":true,"cols":80,"rows":24}`, envID))
	sessionID := created["session_id"].(string)

	serverConn, clientConn := net.Pipe()
	defer clientConn.Close()
	go s.handleDataConn(serverConn, "secret")
	_ = clientConn.SetDeadline(time.Now().Add(15 * time.Second))

	writeFrame := func(kind protocol.Kind, value any) {
		payload, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err := protocol.WriteFrame(clientConn, protocol.Frame{
			Kind:    kind,
			Payload: payload,
		}, protocol.EmbeddedMaxFrame); err != nil {
			t.Fatal(err)
		}
	}
	readFrame := func() protocol.Frame {
		frame, err := protocol.ReadFrame(clientConn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatal(err)
		}
		return frame
	}

	writeFrame(protocol.KindHello, map[string]any{
		"token":    "secret",
		"protocol": map[string]any{"min": 2, "max": 2},
	})
	if ack := readFrame(); ack.Kind != protocol.KindHelloAck {
		t.Fatalf("hello ack kind: %s", ack.Kind)
	}
	writeFrame(protocol.KindOpen, map[string]any{
		"session_id": sessionID,
		"cols":       80,
		"rows":       24,
	})
	if ack := readFrame(); ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}

	if err := protocol.WriteFrame(clientConn, protocol.Frame{
		Kind:    protocol.KindData,
		Payload: []byte("echo service-hello\r"),
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}

	found := false
	for !found {
		frame := readFrame()
		switch frame.Kind {
		case protocol.KindData:
			if strings.Contains(string(frame.Payload), "service-hello") {
				found = true
			}
		case protocol.KindExit:
			t.Fatalf("session exited before output")
		case protocol.KindError:
			t.Fatalf("data error: %s", frame.Payload)
		}
	}
	_ = protocol.WriteFrame(clientConn, protocol.Frame{Kind: protocol.KindClose}, protocol.EmbeddedMaxFrame)
	clientConn.Close()
	mustTool(t, s, "session_close", fmt.Sprintf(`{"session_id":%q}`, sessionID))
}

func TestServiceDataRejectsBadToken(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	serverConn, clientConn := net.Pipe()
	defer clientConn.Close()
	go s.handleDataConn(serverConn, "secret")
	_ = clientConn.SetDeadline(time.Now().Add(5 * time.Second))

	payload, _ := json.Marshal(map[string]any{
		"token":    "wrong",
		"protocol": map[string]any{"min": 2, "max": 2},
	})
	if err := protocol.WriteFrame(clientConn, protocol.Frame{
		Kind:    protocol.KindHello,
		Payload: payload,
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	frame, err := protocol.ReadFrame(clientConn, protocol.EmbeddedMaxFrame)
	if err != nil {
		t.Fatal(err)
	}
	if frame.Kind != protocol.KindError {
		t.Fatalf("expected ERROR, got %s", frame.Kind)
	}
}

func TestServiceHTTPIsolatedFailsClosed(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	s.transport = "service-http"
	handler := serviceHandler{server: s, token: "secret"}
	request := httptest.NewRequest(http.MethodPost, "/mcp", strings.NewReader(
		`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"env_create","arguments":{"profile":"isolated"}}}`))
	request.Header.Set("Authorization", "Bearer secret")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status: %d body=%s", response.Code, response.Body.String())
	}
	var body struct {
		Result struct {
			IsError    bool           `json:"isError"`
			Structured map[string]any `json:"structuredContent"`
		} `json:"result"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if !body.Result.IsError {
		t.Fatalf("isolated without a launcher was accepted: %s", response.Body.String())
	}
	detail, _ := body.Result.Structured["error"].(map[string]any)
	if detail["name"] != "FAILED_PRECONDITION" {
		t.Fatalf("error = %#v", detail)
	}
}
