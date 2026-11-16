package server

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math/big"
	"net"
	"net/http"
	"net/http/httptest"
	"runtime"
	"strings"
	"testing"
	"time"
)

func newNetTestServer(t *testing.T) (*Server, string) {
	t.Helper()
	s := New(Options{Root: t.TempDir()})
	env := mustTool(t, s, "env_create", `{"name":"net"}`)
	id, _ := env["env_id"].(string)
	if id == "" {
		t.Fatal("missing env_id")
	}
	return s, id
}

func netToolData(t *testing.T, result toolResult) map[string]any {
	t.Helper()
	data, ok := decodeTool(t, result)
	if !ok {
		t.Fatalf("tool failed: %#v", result.StructuredContent)
	}
	return data
}

func TestNetDNSRequiresKnownTypeAndEnvironment(t *testing.T) {
	s, id := newNetTestServer(t)

	detail := toolError(t, s.toolNetDNS(json.RawMessage(`{"env_id":"env_missing","name":"example.test"}`)))
	if detail["name"] != "NOT_FOUND" {
		t.Fatalf("missing environment error = %v", detail)
	}

	detail = toolError(t, s.toolNetDNS(json.RawMessage(
		`{"env_id":"`+id+`","name":"example.test","type":"BOGUS"}`)))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("unknown type error = %v", detail)
	}

	detail = toolError(t, s.toolNetDNS(json.RawMessage(`{"env_id":"`+id+`"}`)))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("missing name error = %v", detail)
	}

	detail = toolError(t, s.toolNetDNS(json.RawMessage(
		`{"env_id":"`+id+`","name":"example.test","server":"not-a-host-port"}`)))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("bad server error = %v", detail)
	}
}

func TestNetDNSCustomResolver(t *testing.T) {
	s, id := newNetTestServer(t)
	server := startNetTestDNSServer(t, net.ParseIP("127.0.0.1"))

	args, err := json.Marshal(map[string]any{
		"env_id":     id,
		"name":       "example.test",
		"type":       "A",
		"server":     server,
		"timeout_ms": 2000,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetDNS(args))

	if data["type"] != "A" || data["server"] != server {
		t.Fatalf("unexpected dns metadata: %#v", data)
	}
	values, ok := data["values"].([]any)
	if !ok || len(values) != 1 || values[0] != "127.0.0.1" {
		t.Fatalf("values = %#v", data["values"])
	}
	records, ok := data["records"].([]any)
	if !ok || len(records) != 1 {
		t.Fatalf("records = %#v", data["records"])
	}
	record, _ := records[0].(map[string]any)
	if record["type"] != "A" || record["address"] != "127.0.0.1" {
		t.Fatalf("record = %#v", record)
	}
}

// startNetTestDNSServer answers every A query with the supplied address. It runs
// entirely on the loopback interface and never touches a public resolver.
func startNetTestDNSServer(t *testing.T, address net.IP) string {
	t.Helper()
	conn, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	go func() {
		buffer := make([]byte, 1500)
		for {
			n, peer, err := conn.ReadFrom(buffer)
			if err != nil {
				return
			}
			response := buildNetTestDNSAResponse(buffer[:n], address)
			if response != nil {
				_, _ = conn.WriteTo(response, peer)
			}
		}
	}()
	return conn.LocalAddr().String()
}

func buildNetTestDNSAResponse(query []byte, address net.IP) []byte {
	end, ok := netTestDNSQuestionEnd(query)
	if !ok {
		return nil
	}
	ip4 := address.To4()
	if ip4 == nil {
		return nil
	}
	qtype := binary.BigEndian.Uint16(query[end-4:])
	qclass := binary.BigEndian.Uint16(query[end-2:])

	answer := []byte{}
	answerCount := uint16(0)
	if qtype == 1 && qclass == 1 {
		answer = []byte{
			0xC0, 0x0C, // pointer to the question name
			0x00, 0x01, // A
			0x00, 0x01, // IN
			0x00, 0x00, 0x00, 0x3C, // TTL 60
			0x00, 0x04,
		}
		answer = append(answer, ip4...)
		answerCount = 1
	}

	response := make([]byte, 12, 12+(end-12)+len(answer))
	copy(response, query[:12])
	response = append(response, query[12:end]...)
	response = append(response, answer...)
	binary.BigEndian.PutUint16(response[2:], 0x8180) // response, recursion available
	binary.BigEndian.PutUint16(response[4:], 1)      // QDCOUNT
	binary.BigEndian.PutUint16(response[6:], answerCount)
	binary.BigEndian.PutUint16(response[8:], 0)  // NSCOUNT
	binary.BigEndian.PutUint16(response[10:], 0) // ARCOUNT
	return response
}

func netTestDNSQuestionEnd(query []byte) (int, bool) {
	if len(query) < 12 {
		return 0, false
	}
	offset := 12
	for {
		if offset >= len(query) {
			return 0, false
		}
		length := int(query[offset])
		offset++
		if length == 0 {
			break
		}
		if length&0xC0 != 0 || offset+length > len(query) {
			return 0, false
		}
		offset += length
	}
	if offset+4 > len(query) {
		return 0, false
	}
	return offset + 4, true
}

func TestNetDialEcho(t *testing.T) {
	s, id := newNetTestServer(t)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		_ = conn.SetDeadline(time.Now().Add(2 * time.Second))
		buffer := make([]byte, 4)
		if _, err := io.ReadFull(conn, buffer); err != nil {
			return
		}
		if string(buffer) != "ping" {
			return
		}
		_, _ = conn.Write([]byte("pong"))
	}()

	args, err := json.Marshal(map[string]any{
		"env_id":      id,
		"host":        "127.0.0.1",
		"port":        netTestListenerPort(t, listener),
		"send_base64": base64.StdEncoding.EncodeToString([]byte("ping")),
		"read_bytes":  8,
		"timeout_ms":  2000,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetDial(args))
	select {
	case <-done:
	case <-time.After(3 * time.Second):
		t.Fatal("echo server did not finish; net_dial likely failed to send")
	}

	if data["connected"] != true {
		t.Fatalf("connected = %v", data["connected"])
	}
	if data["received_text"] != "pong" {
		t.Fatalf("received_text = %q", data["received_text"])
	}
	if data["received_bytes"].(float64) != 4 {
		t.Fatalf("received_bytes = %v", data["received_bytes"])
	}
	if data["eof"] != true {
		t.Fatalf("eof = %v", data["eof"])
	}
}

func TestNetDialTLS(t *testing.T) {
	s, id := newNetTestServer(t)
	listener := newNetTestTLSListener(t, nil)

	args, err := json.Marshal(map[string]any{
		"env_id":       id,
		"host":         "127.0.0.1",
		"port":         netTestListenerPort(t, listener),
		"tls":          true,
		"insecure_tls": true,
		"read_bytes":   6,
		"timeout_ms":   2000,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetDial(args))
	if data["connected"] != true || data["tls"] != true {
		t.Fatalf("connection metadata = %#v", data)
	}
	if data["received_text"] != "secure" {
		t.Fatalf("received_text = %q", data["received_text"])
	}
	if data["tls_version"] == "" || data["cipher_suite"] == "" {
		t.Fatalf("missing tls details: %#v", data)
	}
}

func TestNetDialRefusedIsNotAnError(t *testing.T) {
	s, id := newNetTestServer(t)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := netTestListenerPort(t, listener)
	_ = listener.Close()

	args, err := json.Marshal(map[string]any{
		"env_id":     id,
		"host":       "127.0.0.1",
		"port":       port,
		"timeout_ms": 500,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetDial(args))
	if data["connected"] != false {
		t.Fatalf("connected = %v", data["connected"])
	}
	if message, _ := data["error"].(string); message == "" {
		t.Fatalf("missing connection error: %#v", data)
	}
}

func TestNetTLSCollectsAndVerifiesChain(t *testing.T) {
	s, id := newNetTestServer(t)
	listener := newNetTestTLSListener(t, []string{"http/1.1"})

	args, err := json.Marshal(map[string]any{
		"env_id":     id,
		"host":       "127.0.0.1",
		"port":       netTestListenerPort(t, listener),
		"timeout_ms": 2000,
		"alpn":       []string{"http/1.1"},
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetTLS(args))

	if data["connected"] != true {
		t.Fatalf("connected = %v", data["connected"])
	}
	if data["verified"] != false {
		t.Fatalf("self-signed certificate unexpectedly verified: %#v", data)
	}
	if message, _ := data["verify_error"].(string); message == "" {
		t.Fatalf("missing verify_error: %#v", data)
	}
	if data["alpn"] != "http/1.1" {
		t.Fatalf("alpn = %v", data["alpn"])
	}
	certs, ok := data["peer_certificates"].([]any)
	if !ok || len(certs) != 1 {
		t.Fatalf("peer_certificates = %#v", data["peer_certificates"])
	}
	cert, _ := certs[0].(map[string]any)
	if !strings.Contains(cert["pem"].(string), "BEGIN CERTIFICATE") {
		t.Fatalf("pem = %v", cert["pem"])
	}
	if len(cert["sha256_fingerprint"].(string)) != 64 {
		t.Fatalf("fingerprint = %v", cert["sha256_fingerprint"])
	}
	ips, _ := cert["ip_addresses"].([]any)
	if len(ips) != 1 || ips[0] != "127.0.0.1" {
		t.Fatalf("ip_addresses = %#v", cert["ip_addresses"])
	}
}

func TestNetTLSConnectionFailureIsNotAnError(t *testing.T) {
	s, id := newNetTestServer(t)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := netTestListenerPort(t, listener)
	_ = listener.Close()

	args, err := json.Marshal(map[string]any{
		"env_id":     id,
		"host":       "127.0.0.1",
		"port":       port,
		"timeout_ms": 500,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetTLS(args))
	if data["connected"] != false {
		t.Fatalf("connected = %v", data["connected"])
	}
}

func TestNetScanFindsOpenPortsAndBanners(t *testing.T) {
	s, id := newNetTestServer(t)
	bannerListener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = bannerListener.Close() })
	go func() {
		conn, err := bannerListener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		_, _ = conn.Write([]byte("SSH-2.0-test\r\n"))
		time.Sleep(2 * time.Second)
	}()

	plainListener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = plainListener.Close() })
	go func() {
		conn, err := plainListener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		time.Sleep(2 * time.Second)
	}()

	closedListener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	closedPort := netTestListenerPort(t, closedListener)
	_ = closedListener.Close()

	bannerPort := netTestListenerPort(t, bannerListener)
	plainPort := netTestListenerPort(t, plainListener)
	args, err := json.Marshal(map[string]any{
		"env_id":      id,
		"targets":     []string{"127.0.0.1"},
		"ports":       []int{bannerPort, plainPort, closedPort},
		"timeout_ms":  300,
		"concurrency": 4,
		"banner":      true,
	})
	if err != nil {
		t.Fatal(err)
	}
	data := netToolData(t, s.toolNetScan(args))

	if data["probes"].(float64) != 3 {
		t.Fatalf("probes = %v", data["probes"])
	}
	if data["open_count"].(float64) != 2 {
		t.Fatalf("open_count = %v", data["open_count"])
	}
	targets, ok := data["targets"].([]any)
	if !ok || len(targets) != 1 {
		t.Fatalf("targets = %#v", data["targets"])
	}
	target, _ := targets[0].(map[string]any)
	if target["address"] != "127.0.0.1" {
		t.Fatalf("address = %v", target["address"])
	}
	open, _ := target["open"].([]any)
	if len(open) != 2 {
		t.Fatalf("open = %#v", target["open"])
	}
	ports := make([]float64, 0, len(open))
	bannerText := ""
	for _, rawEntry := range open {
		entry, _ := rawEntry.(map[string]any)
		ports = append(ports, entry["port"].(float64))
		if int(entry["port"].(float64)) == bannerPort {
			bannerText, _ = entry["banner_text"].(string)
		}
	}
	if ports[0] > ports[1] {
		t.Fatalf("ports not sorted: %v", ports)
	}
	if !strings.Contains(bannerText, "SSH-2.0-test") {
		t.Fatalf("banner_text = %q", bannerText)
	}
}

func TestNetScanRejectsOversizedScan(t *testing.T) {
	s, id := newNetTestServer(t)
	targets := make([]string, 64)
	for i := range targets {
		targets[i] = fmt.Sprintf("host%d.invalid", i)
	}
	ports := make([]int, 0, 129)
	for port := 1; port <= 129; port++ {
		ports = append(ports, port)
	}
	args, err := json.Marshal(map[string]any{
		"env_id":  id,
		"targets": targets,
		"ports":   ports,
	})
	if err != nil {
		t.Fatal(err)
	}
	detail := toolError(t, s.toolNetScan(args))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("error = %v", detail)
	}
	if message, _ := detail["message"].(string); !strings.Contains(message, "8192") {
		t.Fatalf("message = %v", detail["message"])
	}
}

func TestNetScanParsesPortRange(t *testing.T) {
	ports, err := parsePortSpec("22, 80,443,8000-8002,80")
	if err != nil {
		t.Fatal(err)
	}
	expected := []int{22, 80, 443, 8000, 8001, 8002}
	if len(ports) != len(expected) {
		t.Fatalf("ports = %v", ports)
	}
	for i := range expected {
		if ports[i] != expected[i] {
			t.Fatalf("ports = %v", ports)
		}
	}
	for _, spec := range []string{"0", "70000", "90-80", "abc", ""} {
		if _, err := parsePortSpec(spec); err == nil {
			t.Errorf("accepted invalid spec %q", spec)
		}
	}
}

func TestNetInfoReportsHostInventory(t *testing.T) {
	s, id := newNetTestServer(t)
	data := netToolData(t, s.toolNetInfo(json.RawMessage(`{"env_id":"`+id+`"}`)))

	if data["os"] != runtime.GOOS || data["arch"] != runtime.GOARCH {
		t.Fatalf("platform = %v/%v", data["os"], data["arch"])
	}
	if hostname, _ := data["hostname"].(string); hostname == "" {
		t.Fatalf("missing hostname: %#v", data)
	}
	if _, ok := data["interfaces"].([]any); !ok {
		t.Fatalf("interfaces = %#v", data["interfaces"])
	}
	if _, ok := data["dns_servers"].([]any); !ok {
		t.Fatalf("dns_servers = %#v", data["dns_servers"])
	}
}

func TestWebRequestFollowsRedirectsAndKeepsCookies(t *testing.T) {
	s, id := newNetTestServer(t)
	server := httptest.NewServer(netTestWebMux())
	defer server.Close()

	data := netTestWebRequest(t, s, id, map[string]any{
		"url":        server.URL + "/redirect",
		"timeout_ms": 5000,
		"max_bytes":  1024,
	})
	if data["status"].(float64) != 200 || data["status_text"] != "OK" {
		t.Fatalf("status = %v %v", data["status"], data["status_text"])
	}
	if data["body_text"] != "hello-web" {
		t.Fatalf("body_text = %q", data["body_text"])
	}
	if !strings.HasSuffix(data["final_url"].(string), "/final") {
		t.Fatalf("final_url = %v", data["final_url"])
	}
	redirects, ok := data["redirects"].([]any)
	if !ok || len(redirects) != 1 {
		t.Fatalf("redirects = %#v", data["redirects"])
	}
	redirect, _ := redirects[0].(map[string]any)
	if redirect["status"].(float64) != 302 || redirect["location"] != "/final" {
		t.Fatalf("redirect = %#v", redirect)
	}
	cookies, ok := data["cookies"].([]any)
	if !ok || len(cookies) != 1 {
		t.Fatalf("cookies = %#v", data["cookies"])
	}
	cookie, _ := cookies[0].(map[string]any)
	if cookie["name"] != "session" || cookie["value"] != "abc" {
		t.Fatalf("cookie = %#v", cookie)
	}
	if data["tls"] != nil {
		t.Fatalf("plain http reported tls: %#v", data["tls"])
	}
}

func TestWebRequestSendsJSONAndFormBodies(t *testing.T) {
	s, id := newNetTestServer(t)
	server := httptest.NewServer(netTestWebMux())
	defer server.Close()

	jsonData := netTestWebRequest(t, s, id, map[string]any{
		"url":    server.URL + "/echo",
		"method": "POST",
		"json":   map[string]any{"a": 1},
	})
	if jsonData["body_text"] != `POST|application/json|{"a":1}|` {
		t.Fatalf("json body = %q", jsonData["body_text"])
	}

	formData := netTestWebRequest(t, s, id, map[string]any{
		"url":    server.URL + "/echo",
		"method": "POST",
		"form":   map[string]string{"b": "2", "a": "1"},
	})
	if formData["body_text"] != "POST|application/x-www-form-urlencoded|a=1&b=2|" {
		t.Fatalf("form body = %q", formData["body_text"])
	}
}

func TestWebRequestBoundsAndOptions(t *testing.T) {
	s, id := newNetTestServer(t)
	server := httptest.NewServer(netTestWebMux())
	defer server.Close()

	truncated := netTestWebRequest(t, s, id, map[string]any{
		"url":       server.URL + "/big",
		"max_bytes": 100,
	})
	if truncated["truncated"] != true {
		t.Fatalf("truncated = %v", truncated["truncated"])
	}
	if truncated["bytes"].(float64) != 100 {
		t.Fatalf("bytes = %v", truncated["bytes"])
	}
	decoded, err := base64.StdEncoding.DecodeString(truncated["body_base64"].(string))
	if err != nil || len(decoded) != 100 {
		t.Fatalf("body_base64 length = %d err = %v", len(decoded), err)
	}

	notFollowed := netTestWebRequest(t, s, id, map[string]any{
		"url":              server.URL + "/redirect",
		"follow_redirects": false,
	})
	if notFollowed["status"].(float64) != 302 {
		t.Fatalf("status = %v", notFollowed["status"])
	}
	if !strings.HasSuffix(notFollowed["final_url"].(string), "/redirect") {
		t.Fatalf("final_url = %v", notFollowed["final_url"])
	}

	agent := netTestWebRequest(t, s, id, map[string]any{
		"url":        server.URL + "/ua",
		"user_agent": "msk-test-agent",
	})
	if agent["body_text"] != "msk-test-agent" {
		t.Fatalf("user agent = %q", agent["body_text"])
	}

	detail := toolError(t, s.toolWebRequest(json.RawMessage(
		`{"env_id":"`+id+`","url":"ftp://example.test/file"}`)))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("bad scheme error = %v", detail)
	}
	detail = toolError(t, s.toolWebRequest(json.RawMessage(`{"env_id":"`+id+`"}`)))
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("missing url error = %v", detail)
	}
}

func TestWebRequestTLSAndProxy(t *testing.T) {
	s, id := newNetTestServer(t)

	tlsServer := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "secure-web")
	}))
	tlsServer.Config.ErrorLog = log.New(io.Discard, "", 0)
	defer tlsServer.Close()

	data := netTestWebRequest(t, s, id, map[string]any{
		"url":          tlsServer.URL,
		"insecure_tls": true,
	})
	if data["body_text"] != "secure-web" {
		t.Fatalf("body_text = %q", data["body_text"])
	}
	tlsInfo, ok := data["tls"].(map[string]any)
	if !ok || tlsInfo["version"] == "" {
		t.Fatalf("tls = %#v", data["tls"])
	}

	detail := toolError(t, s.toolWebRequest(json.RawMessage(
		`{"env_id":"`+id+`","url":"`+tlsServer.URL+`"}`)))
	if detail["name"] != "REQUEST_FAILED" {
		t.Fatalf("untrusted tls error = %v", detail)
	}

	proxy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "proxied")
	}))
	defer proxy.Close()
	proxied := netTestWebRequest(t, s, id, map[string]any{
		"url":   "http://example.invalid/",
		"proxy": proxy.URL,
	})
	if proxied["body_text"] != "proxied" {
		t.Fatalf("proxy body = %q", proxied["body_text"])
	}
}

func netTestWebMux() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/redirect", func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, "/final", http.StatusFound)
	})
	mux.HandleFunc("/final", func(w http.ResponseWriter, r *http.Request) {
		http.SetCookie(w, &http.Cookie{Name: "session", Value: "abc", Path: "/"})
		_, _ = io.WriteString(w, "hello-web")
	})
	mux.HandleFunc("/echo", func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		_, _ = fmt.Fprintf(w, "%s|%s|%s|%s", r.Method, r.Header.Get("Content-Type"),
			body, r.Header.Get("X-Test"))
	})
	mux.HandleFunc("/big", func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, strings.Repeat("x", 2000))
	})
	mux.HandleFunc("/ua", func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, r.UserAgent())
	})
	return mux
}

func TestWebRequestRedirectHeaderStrippingAndLimit(t *testing.T) {
	s, id := newNetTestServer(t)
	var seenKey, seenAuth string
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenKey = r.Header.Get("X-Api-Key")
		seenAuth = r.Header.Get("Authorization")
		_, _ = w.Write([]byte("ok"))
	}))
	defer target.Close()
	redirector := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL, http.StatusFound)
	}))
	defer redirector.Close()

	args, _ := json.Marshal(map[string]any{
		"env_id":  id,
		"url":     redirector.URL,
		"headers": map[string]string{"X-Api-Key": "secret", "Authorization": "Bearer secret"},
	})
	data := netToolData(t, s.toolWebRequest(args))
	if data["status"].(float64) != 200 {
		t.Fatalf("status = %v", data["status"])
	}
	if seenKey != "" || seenAuth != "" {
		t.Fatalf("cross-origin redirect leaked headers: key=%q auth=%q", seenKey, seenAuth)
	}
	if redirects, _ := data["redirects"].([]any); len(redirects) != 1 {
		t.Fatalf("redirects = %v", data["redirects"])
	}

	noFollow, _ := json.Marshal(map[string]any{
		"env_id":           id,
		"url":              redirector.URL,
		"follow_redirects": false,
	})
	data = netToolData(t, s.toolWebRequest(noFollow))
	if data["status"].(float64) != 302 {
		t.Fatalf("no-follow status = %v", data["status"])
	}

	oneHop, _ := json.Marshal(map[string]any{
		"env_id":        id,
		"url":           redirector.URL,
		"max_redirects": 1,
	})
	data = netToolData(t, s.toolWebRequest(oneHop))
	if data["status"].(float64) != 200 {
		t.Fatalf("one-hop status = %v", data["status"])
	}
}

func netTestWebRequest(t *testing.T, s *Server, envID string, args map[string]any) map[string]any {
	t.Helper()
	args["env_id"] = envID
	raw, err := json.Marshal(args)
	if err != nil {
		t.Fatal(err)
	}
	return netToolData(t, s.toolWebRequest(raw))
}

func netTestListenerPort(t *testing.T, listener net.Listener) int {
	t.Helper()
	tcp, ok := listener.Addr().(*net.TCPAddr)
	if !ok {
		t.Fatalf("listener address type: %T", listener.Addr())
	}
	return tcp.Port
}

func newNetTestTLSListener(t *testing.T, nextProtos []string) net.Listener {
	t.Helper()
	certificate := newNetTestCertificate(t)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func(conn net.Conn) {
				defer conn.Close()
				tlsConn := tls.Server(conn, &tls.Config{
					Certificates: []tls.Certificate{certificate},
					NextProtos:   nextProtos,
				})
				if err := tlsConn.Handshake(); err != nil {
					return
				}
				_, _ = io.WriteString(tlsConn, "secure")
			}(conn)
		}
	}()
	return listener
}

func newNetTestCertificate(t *testing.T) tls.Certificate {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "localhost"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(time.Hour),
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageCertSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		BasicConstraintsValid: true,
		IsCA:                  true,
		DNSNames:              []string{"localhost"},
		IPAddresses:           []net.IP{net.ParseIP("127.0.0.1")},
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	return tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key}
}
