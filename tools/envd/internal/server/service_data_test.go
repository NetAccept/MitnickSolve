package server

import (
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"net"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func dataSession(t *testing.T, capacity int) (*Server, string) {
	t.Helper()
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})
	env := mustTool(t, s, "env_create", `{"name":"data"}`)
	envID := env["env_id"].(string)
	args := map[string]any{
		"env_id":              envID,
		"pty":                 true,
		"cols":                80,
		"rows":                24,
		"output_buffer_bytes": capacity,
	}
	payload, _ := json.Marshal(args)
	created := mustTool(t, s, "session_create", string(payload))
	sessionID, _ := created["session_id"].(string)
	if sessionID == "" {
		t.Fatal("missing session id")
	}
	t.Cleanup(func() {
		_ = s.callTool("session_close", json.RawMessage(`{"session_id":"`+sessionID+`"}`))
	})
	return s, sessionID
}

func drainControlOutput(t *testing.T, s *Server, sessionID string, done func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if done() {
			return
		}
		payload, _ := json.Marshal(map[string]any{
			"session_id": sessionID,
			"max_bytes":  1 << 20,
			"timeout_ms": 0,
		})
		_ = mustTool(t, s, "session_read", string(payload))
		time.Sleep(10 * time.Millisecond)
	}
}

func writeLargeFile(t *testing.T, s *Server, sessionID string) {
	t.Helper()
	session, err := s.getSession(sessionID)
	if err != nil {
		t.Fatal(err)
	}
	content := strings.Repeat("0123456789abcdef", 12000)
	payload, _ := json.Marshal(map[string]any{
		"env_id": session.EnvID,
		"path":   "big.txt",
		"data":   base64.StdEncoding.EncodeToString([]byte(content)),
		"atomic": true,
	})
	mustTool(t, s, "write_file", string(payload))
}

func openDataView(t *testing.T, s *Server, token, sessionID string, since *uint64) (net.Conn, protocol.Frame) {
	t.Helper()
	serverConn, clientConn := net.Pipe()
	go s.handleDataConn(serverConn, token)
	_ = clientConn.SetDeadline(time.Now().Add(15 * time.Second))

	writeJSON := func(kind protocol.Kind, value any) {
		payload, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err := protocol.WriteFrame(clientConn, protocol.Frame{Kind: kind, Payload: payload}, protocol.EmbeddedMaxFrame); err != nil {
			t.Fatal(err)
		}
	}
	writeJSON(protocol.KindHello, map[string]any{
		"token":    token,
		"protocol": map[string]any{"min": 2, "max": 2},
	})
	if ack, err := protocol.ReadFrame(clientConn, protocol.EmbeddedMaxFrame); err != nil || ack.Kind != protocol.KindHelloAck {
		t.Fatalf("hello ack: kind=%v err=%v", ack.Kind, err)
	}
	open := map[string]any{"session_id": sessionID, "cols": 80, "rows": 24}
	if since != nil {
		open["since_seq"] = *since
	}
	writeJSON(protocol.KindOpen, open)
	response, err := protocol.ReadFrame(clientConn, protocol.EmbeddedMaxFrame)
	if err != nil {
		t.Fatal(err)
	}
	return clientConn, response
}

func writeSessionData(t *testing.T, s *Server, sessionID, data string) {
	t.Helper()
	payload, _ := json.Marshal(map[string]any{
		"session_id":  sessionID,
		"data_base64": base64.StdEncoding.EncodeToString([]byte(data)),
	})
	mustTool(t, s, "session_write", string(payload))
}

func readDataUntil(t *testing.T, conn net.Conn, needle string) ([]byte, uint64) {
	t.Helper()
	var output []byte
	var end uint64
	for {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("read data: %v (output %q)", err, output)
		}
		switch frame.Kind {
		case protocol.KindData:
			output = append(output, frame.Payload...)
			end = frame.Seq + uint64(len(frame.Payload))
			if strings.Contains(string(output), needle) {
				return output, end
			}
		case protocol.KindError:
			t.Fatalf("data view error: %s", frame.Payload)
		case protocol.KindExit:
			t.Fatalf("session exited before %q: %q", needle, output)
		}
	}
}

func TestServiceDataReconnectFromCursor(t *testing.T) {
	s, sessionID := dataSession(t, 1<<20)
	first, ack := openDataView(t, s, "secret", sessionID, nil)
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}
	writeSessionData(t, s, sessionID, "echo first\r")
	_, cursor := readDataUntil(t, first, "first")
	first.Close()
	writeSessionData(t, s, sessionID, "echo second\r")
	time.Sleep(200 * time.Millisecond)

	second, ack := openDataView(t, s, "secret", sessionID, &cursor)
	defer second.Close()
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("reconnect open: %s", ack.Payload)
	}
	output, _ := readDataUntil(t, second, "second")
	if !strings.Contains(string(output), "second") {
		t.Fatalf("missing detached output: %q", output)
	}
}

func TestServiceDataCreditBackpressure(t *testing.T) {
	s, sessionID := dataSession(t, 1<<20)
	conn, ack := openDataView(t, s, "secret", sessionID, nil)
	defer conn.Close()
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}
	writeLargeFile(t, s, sessionID)
	command := "cat big.txt\n"
	if runtime.GOOS == "windows" {
		command = "type big.txt\r"
	}
	writeSessionData(t, s, sessionID, command)

	received := 0
	for received < 65536 {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("read before credit: %v", err)
		}
		if frame.Kind == protocol.KindError {
			t.Fatalf("data error: %s", frame.Payload)
		}
		if frame.Kind == protocol.KindData {
			received += len(frame.Payload)
		}
	}
	_ = conn.SetReadDeadline(time.Now().Add(300 * time.Millisecond))
	if _, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame); err == nil {
		t.Fatal("received DATA beyond the initial credit")
	} else if netErr, ok := err.(net.Error); !ok || !netErr.Timeout() {
		t.Fatalf("expected read timeout, got %v", err)
	}
	_ = conn.SetReadDeadline(time.Now().Add(15 * time.Second))

	var credit [8]byte
	binary.BigEndian.PutUint64(credit[:], 65536)
	if err := protocol.WriteFrame(conn, protocol.Frame{Kind: protocol.KindCredit, Payload: credit[:]}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	for received < 70000 {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("read after credit: %v", err)
		}
		if frame.Kind == protocol.KindError {
			t.Fatalf("data error: %s", frame.Payload)
		}
		if frame.Kind == protocol.KindData {
			received += len(frame.Payload)
		}
	}
}

func TestServiceDataSeekReplaysAndRejectsStale(t *testing.T) {
	s, sessionID := dataSession(t, 1<<20)
	conn, ack := openDataView(t, s, "secret", sessionID, nil)
	defer conn.Close()
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}
	writeSessionData(t, s, sessionID, "echo seek-data")
	readDataUntil(t, conn, "seek-data")
	writeSessionData(t, s, sessionID, "echo second-marker")
	readDataUntil(t, conn, "second-marker")

	var seek [8]byte
	binary.BigEndian.PutUint64(seek[:], 0)
	if err := protocol.WriteFrame(conn, protocol.Frame{
		Kind: protocol.KindSeek, Payload: seek[:],
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	replayed, _ := readDataUntil(t, conn, "seek-data")
	if !strings.Contains(string(replayed), "second-marker") {
		t.Fatalf("seek replay missing later output: %q", replayed)
	}

	binary.BigEndian.PutUint64(seek[:], 1<<40)
	if err := protocol.WriteFrame(conn, protocol.Frame{
		Kind: protocol.KindSeek, Payload: seek[:],
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	for {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("expected STALE error, got read error: %v", err)
		}
		if frame.Kind == protocol.KindError {
			var payload struct {
				Code int `json:"code"`
			}
			if err := json.Unmarshal(frame.Payload, &payload); err != nil {
				t.Fatal(err)
			}
			if payload.Code != 14 {
				t.Fatalf("expected STALE code 14, got %d", payload.Code)
			}
			return
		}
	}
}

func TestServiceDataInputCredit(t *testing.T) {
	s, sessionID := dataSession(t, 1<<20)
	conn, ack := openDataView(t, s, "secret", sessionID, nil)
	defer conn.Close()
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}

	// Consume most of the initial 64 KiB input window; the server should
	// grant enough credit to return to the full window.
	payload := make([]byte, 40<<10)
	for index := range payload {
		payload[index] = ' '
	}
	if err := protocol.WriteFrame(conn, protocol.Frame{
		Kind: protocol.KindData, Payload: payload,
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	granted := false
	for !granted {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("read credit: %v", err)
		}
		switch frame.Kind {
		case protocol.KindCredit:
			if len(frame.Payload) != 8 {
				t.Fatalf("credit payload length: %d", len(frame.Payload))
			}
			granted = true
		case protocol.KindData:
			// PTY echo of the typed spaces.
		case protocol.KindError:
			t.Fatalf("unexpected error: %s", frame.Payload)
		}
	}

	// A frame larger than the available window is rejected.
	large := make([]byte, 80<<10)
	if err := protocol.WriteFrame(conn, protocol.Frame{
		Kind: protocol.KindData, Payload: large,
	}, protocol.EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	for {
		frame, err := protocol.ReadFrame(conn, protocol.EmbeddedMaxFrame)
		if err != nil {
			t.Fatalf("expected credit error, got read error: %v", err)
		}
		if frame.Kind == protocol.KindError {
			return
		}
	}
}

func TestServiceDataReportsGapBeforeFirstSeq(t *testing.T) {
	s, sessionID := dataSession(t, 64<<10)
	conn, ack := openDataView(t, s, "secret", sessionID, nil)
	if ack.Kind != protocol.KindOpenAck {
		t.Fatalf("open ack kind: %s", ack.Kind)
	}
	writeLargeFile(t, s, sessionID)
	command := "cat big.txt\n"
	if runtime.GOOS == "windows" {
		command = "type big.txt\r"
	}
	writeSessionData(t, s, sessionID, command)
	_, _ = readDataUntil(t, conn, "0123456789abcdef")
	conn.Close()

	session, _ := s.getSession(sessionID)
	drainControlOutput(t, s, sessionID, func() bool {
		first, _, _, _ := session.replay.bounds()
		return first > 0
	})
	first, last, finished, _ := session.replay.bounds()
	if first == 0 {
		t.Fatalf("output was not trimmed; first=%d last=%d finished=%v closed=%v chunks=%d/%d",
			first, last, finished, session.closed, len(session.chunks), cap(session.chunks))
	}

	zero := uint64(0)
	gapConn, response := openDataView(t, s, "secret", sessionID, &zero)
	defer gapConn.Close()
	if response.Kind != protocol.KindError {
		t.Fatalf("expected GAP error, got %s %s", response.Kind, response.Payload)
	}
	var errPayload struct {
		Code int `json:"code"`
	}
	if err := json.Unmarshal(response.Payload, &errPayload); err != nil {
		t.Fatal(err)
	}
	if errPayload.Code != 13 {
		t.Fatalf("expected code 13, got %d", errPayload.Code)
	}
}
