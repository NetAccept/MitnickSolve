package server

import (
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/cookiejar"
	"net/url"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

// clampMillis applies a default and a hard range to a millisecond argument.
// Zero and negative values mean "unspecified", matching the other tools.
func clampMillis(value, fallback, min, max int64) int64 {
	if value <= 0 {
		return fallback
	}
	if value < min {
		return min
	}
	if value > max {
		return max
	}
	return value
}

func tlsVersionName(version uint16) string {
	switch version {
	case tls.VersionTLS13:
		return "TLS 1.3"
	case tls.VersionTLS12:
		return "TLS 1.2"
	case tls.VersionTLS11:
		return "TLS 1.1"
	case tls.VersionTLS10:
		return "TLS 1.0"
	default:
		return fmt.Sprintf("0x%04x", version)
	}
}

func tlsCipherSuite(id uint16) string {
	if id == 0 {
		return ""
	}
	return tls.CipherSuiteName(id)
}

func certFingerprint(cert *x509.Certificate) string {
	sum := sha256.Sum256(cert.Raw)
	return hex.EncodeToString(sum[:])
}

func certificatePEM(cert *x509.Certificate) string {
	block := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: cert.Raw})
	return string(block)
}

func validText(data []byte) (string, bool) {
	if !utf8.Valid(data) {
		return "", false
	}
	return string(data), true
}

// toolNetDNS resolves a name through the system resolver or a caller-supplied
// DNS server. The custom server is useful on hosts whose resolver configuration
// is not trusted for the target zone.
func (s *Server) toolNetDNS(raw json.RawMessage) toolResult {
	var args struct {
		EnvID     string `json:"env_id"`
		Name      string `json:"name"`
		Type      string `json:"type"`
		Server    string `json:"server"`
		TimeoutMS int64  `json:"timeout_ms"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	args.Name = strings.TrimSpace(args.Name)
	if args.Name == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "name is required")
	}
	if err := s.authorizeNetHost(args.Name); err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	args.Type = strings.ToUpper(strings.TrimSpace(args.Type))
	if args.Type == "" {
		args.Type = "A"
	}
	switch args.Type {
	case "A", "AAAA", "CNAME", "MX", "NS", "TXT", "SRV", "PTR":
	default:
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"unknown record type: "+args.Type)
	}
	if args.Server != "" {
		host, _, err := net.SplitHostPort(args.Server)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"server must be host:port: "+err.Error())
		}
		if err := s.authorizeNetHost(host); err != nil {
			return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
		}
	}
	timeout := clampMillis(args.TimeoutMS, 5000, 100, 30000)

	resolver := net.DefaultResolver
	if args.Server != "" {
		server := args.Server
		dialer := &net.Dialer{Timeout: time.Duration(timeout) * time.Millisecond}
		resolver = &net.Resolver{
			PreferGo: true,
			Dial: func(ctx context.Context, network, _ string) (net.Conn, error) {
				return dialer.DialContext(ctx, network, server)
			},
		}
	}

	ctx, cancel := context.WithTimeout(context.Background(),
		time.Duration(timeout)*time.Millisecond)
	defer cancel()

	started := time.Now()
	values, records, err := resolveDNS(ctx, resolver, args.Name, args.Type)
	if err != nil {
		message := fmt.Sprintf("%s lookup for %s failed: %v", args.Type, args.Name, err)
		var dnsErr *net.DNSError
		switch {
		case errors.As(err, &dnsErr) && dnsErr.IsNotFound:
			return errResult(codeNotFound, "NOT_FOUND", message)
		case errors.Is(err, context.DeadlineExceeded):
			return errResult(codeDeadlineExceeded, "DEADLINE_EXCEEDED", message)
		case errors.As(err, &dnsErr) && dnsErr.IsTimeout:
			return errResult(codeDeadlineExceeded, "DEADLINE_EXCEEDED", message)
		default:
			return errResult(codeInternal, "DNS_FAILURE", message)
		}
	}
	if values == nil {
		values = []string{}
	}
	if records == nil {
		records = []map[string]any{}
	}

	return okResult(fmt.Sprintf("%s %s: %d record(s)", args.Name, args.Type, len(records)),
		map[string]any{
			"name":        args.Name,
			"type":        args.Type,
			"server":      args.Server,
			"duration_ms": time.Since(started).Milliseconds(),
			"values":      values,
			"records":     records,
		})
}

func resolveDNS(
	ctx context.Context,
	resolver *net.Resolver,
	name, recordType string,
) ([]string, []map[string]any, error) {
	values := []string{}
	records := []map[string]any{}
	switch recordType {
	case "A", "AAAA":
		network := "ip4"
		if recordType == "AAAA" {
			network = "ip6"
		}
		ips, err := resolver.LookupIP(ctx, network, name)
		if err != nil {
			return nil, nil, err
		}
		for _, ip := range ips {
			address := ip.String()
			values = append(values, address)
			records = append(records, map[string]any{
				"type":    recordType,
				"address": address,
			})
		}
	case "CNAME":
		target, err := resolver.LookupCNAME(ctx, name)
		if err != nil {
			return nil, nil, err
		}
		values = append(values, target)
		records = append(records, map[string]any{"type": "CNAME", "target": target})
	case "MX":
		mxs, err := resolver.LookupMX(ctx, name)
		if err != nil {
			return nil, nil, err
		}
		for _, mx := range mxs {
			values = append(values, mx.Host)
			records = append(records, map[string]any{
				"type":       "MX",
				"preference": mx.Pref,
				"host":       mx.Host,
			})
		}
	case "NS":
		nss, err := resolver.LookupNS(ctx, name)
		if err != nil {
			return nil, nil, err
		}
		for _, ns := range nss {
			values = append(values, ns.Host)
			records = append(records, map[string]any{"type": "NS", "host": ns.Host})
		}
	case "TXT":
		texts, err := resolver.LookupTXT(ctx, name)
		if err != nil {
			return nil, nil, err
		}
		for _, text := range texts {
			values = append(values, text)
			records = append(records, map[string]any{"type": "TXT", "text": text})
		}
	case "SRV":
		_, srvs, err := resolver.LookupSRV(ctx, "", "", name)
		if err != nil {
			return nil, nil, err
		}
		for _, srv := range srvs {
			values = append(values, srv.Target)
			records = append(records, map[string]any{
				"type":     "SRV",
				"target":   srv.Target,
				"port":     srv.Port,
				"priority": srv.Priority,
				"weight":   srv.Weight,
			})
		}
	case "PTR":
		names, err := resolver.LookupAddr(ctx, name)
		if err != nil {
			return nil, nil, err
		}
		for _, ptr := range names {
			values = append(values, ptr)
			records = append(records, map[string]any{"type": "PTR", "name": ptr})
		}
	default:
		return nil, nil, fmt.Errorf("unknown record type: %s", recordType)
	}
	return values, records, nil
}

func (s *Server) toolNetDial(raw json.RawMessage) toolResult {
	var args struct {
		EnvID       string   `json:"env_id"`
		Host        string   `json:"host"`
		Port        int      `json:"port"`
		SendBase64  string   `json:"send_base64"`
		ReadBytes   int      `json:"read_bytes"`
		TimeoutMS   int64    `json:"timeout_ms"`
		TLS         bool     `json:"tls"`
		ServerName  string   `json:"server_name"`
		InsecureTLS bool     `json:"insecure_tls"`
		ALPN        []string `json:"alpn"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	args.Host = strings.TrimSpace(args.Host)
	if args.Host == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "host is required")
	}
	if err := s.authorizeNetHost(args.Host); err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if args.Port < 1 || args.Port > 65535 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "port must be between 1 and 65535")
	}
	if args.ReadBytes < 0 || args.ReadBytes > 65536 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"read_bytes must be between 0 and 65536")
	}
	if args.ReadBytes == 0 {
		args.ReadBytes = 1024
	}
	if len(args.SendBase64) > 90_000 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "send_base64 exceeds 64 KiB")
	}
	var send []byte
	if args.SendBase64 != "" {
		decoded, err := base64.StdEncoding.DecodeString(args.SendBase64)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"send_base64: "+err.Error())
		}
		if len(decoded) > 64<<10 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "send_base64 exceeds 64 KiB")
		}
		send = decoded
	}
	timeout := clampMillis(args.TimeoutMS, 5000, 100, 30000)

	address := net.JoinHostPort(args.Host, strconv.Itoa(args.Port))
	started := time.Now()
	conn, err := net.DialTimeout("tcp", address, time.Duration(timeout)*time.Millisecond)
	if err != nil {
		return okResult("connection failed", map[string]any{
			"connected":   false,
			"error":       err.Error(),
			"host":        args.Host,
			"port":        args.Port,
			"duration_ms": time.Since(started).Milliseconds(),
		})
	}
	defer conn.Close()
	latency := time.Since(started).Milliseconds()

	tlsVersion := ""
	cipherSuite := ""
	negotiatedALPN := ""
	useTLS := false
	if args.TLS {
		serverName := args.ServerName
		if serverName == "" {
			serverName = args.Host
		}
		tlsConn := tls.Client(conn, &tls.Config{
			InsecureSkipVerify: args.InsecureTLS,
			ServerName:         serverName,
			NextProtos:         args.ALPN,
		})
		_ = conn.SetDeadline(time.Now().Add(time.Duration(timeout) * time.Millisecond))
		if err := tlsConn.Handshake(); err != nil {
			return okResult("tls handshake failed", map[string]any{
				"connected":   false,
				"error":       "tls handshake: " + err.Error(),
				"host":        args.Host,
				"port":        args.Port,
				"duration_ms": time.Since(started).Milliseconds(),
				"latency_ms":  latency,
				"tls":         true,
			})
		}
		conn = tlsConn
		useTLS = true
		state := tlsConn.ConnectionState()
		tlsVersion = tlsVersionName(state.Version)
		cipherSuite = tlsCipherSuite(state.CipherSuite)
		negotiatedALPN = state.NegotiatedProtocol
	}

	var writeErr error
	if len(send) > 0 {
		_ = conn.SetWriteDeadline(time.Now().Add(time.Duration(timeout) * time.Millisecond))
		if _, err := conn.Write(send); err != nil {
			writeErr = err
		}
	}

	received := make([]byte, 0, args.ReadBytes)
	readWindow := time.Duration(timeout) * time.Millisecond
	if readWindow > 2*time.Second {
		readWindow = 2 * time.Second
	}
	_ = conn.SetReadDeadline(time.Now().Add(readWindow))
	buffer := make([]byte, 4096)
	eof := false
	var readErr error
	if writeErr == nil {
		for len(received) < args.ReadBytes {
			want := args.ReadBytes - len(received)
			if want > len(buffer) {
				want = len(buffer)
			}
			n, err := conn.Read(buffer[:want])
			if n > 0 {
				received = append(received, buffer[:n]...)
			}
			if err != nil {
				if errors.Is(err, io.EOF) {
					eof = true
				} else {
					readErr = err
				}
				break
			}
		}
	}

	data := map[string]any{
		"connected":       true,
		"host":            args.Host,
		"port":            args.Port,
		"duration_ms":     time.Since(started).Milliseconds(),
		"latency_ms":      latency,
		"tls":             useTLS,
		"tls_version":     tlsVersion,
		"cipher_suite":    cipherSuite,
		"alpn":            negotiatedALPN,
		"received_base64": base64.StdEncoding.EncodeToString(received),
		"received_bytes":  len(received),
		"eof":             eof,
	}
	if text, ok := validText(received); ok {
		data["received_text"] = text
	}
	if writeErr != nil {
		data["error"] = "write: " + writeErr.Error()
	} else if readErr != nil {
		data["error"] = "read: " + readErr.Error()
	}
	summary := fmt.Sprintf("connected to %s", address)
	if useTLS {
		summary = fmt.Sprintf("connected to %s over %s", address, tlsVersion)
	}
	return okResult(summary, data)
}

func (s *Server) toolNetTLS(raw json.RawMessage) toolResult {
	var args struct {
		EnvID      string   `json:"env_id"`
		Host       string   `json:"host"`
		Port       int      `json:"port"`
		ServerName string   `json:"server_name"`
		TimeoutMS  int64    `json:"timeout_ms"`
		ALPN       []string `json:"alpn"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	args.Host = strings.TrimSpace(args.Host)
	if args.Host == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "host is required")
	}
	if err := s.authorizeNetHost(args.Host); err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if args.Port == 0 {
		args.Port = 443
	}
	if args.Port < 1 || args.Port > 65535 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "port must be between 1 and 65535")
	}
	timeout := clampMillis(args.TimeoutMS, 5000, 100, 30000)
	verifyName := args.ServerName
	if verifyName == "" {
		verifyName = args.Host
	}
	serverName := args.ServerName
	if serverName == "" {
		serverName = args.Host
	}

	address := net.JoinHostPort(args.Host, strconv.Itoa(args.Port))
	started := time.Now()
	conn, err := net.DialTimeout("tcp", address, time.Duration(timeout)*time.Millisecond)
	if err != nil {
		return okResult("connection failed", map[string]any{
			"connected":         false,
			"error":             err.Error(),
			"host":              args.Host,
			"port":              args.Port,
			"duration_ms":       time.Since(started).Milliseconds(),
			"verified":          false,
			"verify_error":      "",
			"peer_certificates": []map[string]any{},
		})
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Duration(timeout) * time.Millisecond))

	tlsConn := tls.Client(conn, &tls.Config{
		InsecureSkipVerify: true,
		ServerName:         serverName,
		NextProtos:         args.ALPN,
	})
	if err := tlsConn.Handshake(); err != nil {
		return okResult("tls handshake failed", map[string]any{
			"connected":         false,
			"error":             "tls handshake: " + err.Error(),
			"host":              args.Host,
			"port":              args.Port,
			"duration_ms":       time.Since(started).Milliseconds(),
			"verified":          false,
			"verify_error":      "",
			"peer_certificates": []map[string]any{},
		})
	}
	state := tlsConn.ConnectionState()

	certs := make([]map[string]any, 0, len(state.PeerCertificates))
	pemBudget := 32 << 10
	certsTruncated := len(state.PeerCertificates) > 8
	pemTruncated := false
	for index, cert := range state.PeerCertificates {
		if index >= 8 {
			break
		}
		dnsNames := append([]string{}, cert.DNSNames...)
		ipAddresses := make([]string, 0, len(cert.IPAddresses))
		for _, ip := range cert.IPAddresses {
			ipAddresses = append(ipAddresses, ip.String())
		}
		pem := ""
		if encoded := certificatePEM(cert); len(encoded) <= pemBudget {
			pem = encoded
			pemBudget -= len(encoded)
		} else {
			pemTruncated = true
		}
		certs = append(certs, map[string]any{
			"subject":             cert.Subject.String(),
			"issuer":              cert.Issuer.String(),
			"serial":              cert.SerialNumber.String(),
			"not_before":          cert.NotBefore.UTC().Format(time.RFC3339),
			"not_after":           cert.NotAfter.UTC().Format(time.RFC3339),
			"dns_names":           dnsNames,
			"ip_addresses":        ipAddresses,
			"is_ca":               cert.IsCA,
			"signature_algorithm": cert.SignatureAlgorithm.String(),
			"sha256_fingerprint":  certFingerprint(cert),
			"pem":                 pem,
		})
	}

	verified := false
	verifyError := ""
	if len(state.PeerCertificates) == 0 {
		verifyError = "no peer certificates"
	} else {
		var roots *x509.CertPool
		if pool, err := x509.SystemCertPool(); err == nil {
			roots = pool
		}
		intermediates := x509.NewCertPool()
		for _, cert := range state.PeerCertificates[1:] {
			intermediates.AddCert(cert)
		}
		_, err := state.PeerCertificates[0].Verify(x509.VerifyOptions{
			DNSName:       verifyName,
			Intermediates: intermediates,
			Roots:         roots,
		})
		if err != nil {
			verifyError = err.Error()
		} else {
			verified = true
		}
	}

	return okResult(fmt.Sprintf("tls %s on %s", tlsVersionName(state.Version), address),
		map[string]any{
			"connected":              true,
			"host":                   args.Host,
			"port":                   args.Port,
			"duration_ms":            time.Since(started).Milliseconds(),
			"version":                tlsVersionName(state.Version),
			"cipher_suite":           tlsCipherSuite(state.CipherSuite),
			"alpn":                   state.NegotiatedProtocol,
			"verified":               verified,
			"verify_error":           verifyError,
			"peer_certificates":      certs,
			"certificates_truncated": certsTruncated,
			"pem_truncated":          pemTruncated,
		})
}

func (s *Server) toolNetScan(raw json.RawMessage) toolResult {
	var args struct {
		EnvID       string   `json:"env_id"`
		Targets     []string `json:"targets"`
		Ports       []int    `json:"ports"`
		PortRange   string   `json:"port_range"`
		TimeoutMS   int64    `json:"timeout_ms"`
		Concurrency int      `json:"concurrency"`
		Banner      bool     `json:"banner"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	if len(args.Targets) == 0 || len(args.Targets) > 64 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"targets must contain between 1 and 64 entries")
	}

	hosts := make([]string, 0, len(args.Targets))
	seenHosts := make(map[string]bool, len(args.Targets))
	for _, target := range args.Targets {
		target = strings.TrimSpace(target)
		if target == "" {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "targets must not be empty")
		}
		if err := s.authorizeNetHost(target); err != nil {
			return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
		}
		key := strings.ToLower(target)
		if seenHosts[key] {
			continue
		}
		seenHosts[key] = true
		hosts = append(hosts, target)
	}

	ports := make([]int, 0, len(args.Ports)+16)
	seenPorts := make(map[int]bool)
	addPort := func(port int) error {
		if port < 1 || port > 65535 {
			return fmt.Errorf("port %d is outside 1-65535", port)
		}
		if !seenPorts[port] {
			seenPorts[port] = true
			ports = append(ports, port)
		}
		return nil
	}
	for _, port := range args.Ports {
		if err := addPort(port); err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
		}
	}
	if strings.TrimSpace(args.PortRange) != "" {
		parsed, err := parsePortSpec(args.PortRange)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
		}
		for _, port := range parsed {
			if err := addPort(port); err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
			}
		}
	}
	if len(ports) == 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"ports or port_range is required")
	}
	sort.Ints(ports)

	if len(hosts)*len(ports) > 8192 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			fmt.Sprintf("scan would perform %d probes; narrow targets or ports to at most 8192",
				len(hosts)*len(ports)))
	}

	timeout := clampMillis(args.TimeoutMS, 500, 50, 5000)
	concurrency := args.Concurrency
	if concurrency <= 0 {
		concurrency = 32
	}
	if concurrency < 1 {
		concurrency = 1
	}
	if concurrency > 128 {
		concurrency = 128
	}

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	type scanTarget struct {
		host    string
		address string
		err     string
		open    []map[string]any
	}
	targets := make([]scanTarget, len(hosts))
	for i, host := range hosts {
		targets[i].host = host
		if ctx.Err() != nil {
			break
		}
		address, err := firstScanAddress(ctx, host)
		if err != nil {
			targets[i].err = err.Error()
			continue
		}
		targets[i].address = address
		targets[i].open = []map[string]any{}
	}

	type probe struct {
		target int
		port   int
	}
	jobs := make(chan probe)
	var wg sync.WaitGroup
	var mu sync.Mutex
	probes := 0
	truncated := false
	resultBudget := 160 << 10
	resultBytes := 0
	const maxOpenEntries = 2048
	dialer := &net.Dialer{}
	for i := 0; i < concurrency; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for job := range jobs {
				if ctx.Err() != nil {
					continue
				}
				// Pace individual probes so a single tool call cannot become a
				// flood when concurrency is high.
				select {
				case <-ctx.Done():
					continue
				case <-time.After(5 * time.Millisecond):
				}
				address := net.JoinHostPort(targets[job.target].address,
					strconv.Itoa(job.port))
				probeStarted := time.Now()
				conn, err := dialer.DialContext(ctx, "tcp", address)
				if err != nil {
					continue
				}
				entry := map[string]any{
					"port":       job.port,
					"latency_ms": time.Since(probeStarted).Milliseconds(),
				}
				bannerBytes := 0
				if args.Banner {
					entry["banner_base64"] = ""
					bannerTimeout := time.Duration(timeout) * time.Millisecond
					if bannerTimeout > time.Second {
						bannerTimeout = time.Second
					}
					_ = conn.SetReadDeadline(time.Now().Add(bannerTimeout))
					buffer := make([]byte, 256)
					if n, _ := conn.Read(buffer); n > 0 {
						bannerBytes = n
						entry["banner_base64"] = base64.StdEncoding.EncodeToString(buffer[:n])
						if text, ok := validText(buffer[:n]); ok {
							entry["banner_text"] = text
						}
					}
				}
				_ = conn.Close()
				entryBytes := 96 + bannerBytes*2
				mu.Lock()
				if len(targets[job.target].open) >= maxOpenEntries ||
					resultBytes+entryBytes > resultBudget {
					truncated = true
				} else {
					targets[job.target].open = append(targets[job.target].open, entry)
					resultBytes += entryBytes
				}
				mu.Unlock()
			}
		}()
	}

	started := time.Now()
	for i := range targets {
		if targets[i].address == "" {
			continue
		}
		for _, port := range ports {
			jobs <- probe{target: i, port: port}
			probes++
		}
	}
	close(jobs)
	wg.Wait()

	openCount := 0
	out := make([]map[string]any, 0, len(targets))
	for i := range targets {
		sort.Slice(targets[i].open, func(a, b int) bool {
			return targets[i].open[a]["port"].(int) < targets[i].open[b]["port"].(int)
		})
		openCount += len(targets[i].open)
		entry := map[string]any{
			"host":    targets[i].host,
			"address": targets[i].address,
			"open":    targets[i].open,
		}
		if targets[i].err != "" {
			entry["error"] = targets[i].err
		}
		out = append(out, entry)
	}

	return okResult(fmt.Sprintf("%d open port(s) across %d target(s)", openCount, len(out)),
		map[string]any{
			"targets":     out,
			"probes":      probes,
			"open_count":  openCount,
			"duration_ms": time.Since(started).Milliseconds(),
			"truncated":   truncated || ctx.Err() != nil,
			"timed_out":   ctx.Err() == context.DeadlineExceeded,
		})
}

func parsePortSpec(spec string) ([]int, error) {
	ports := []int{}
	seen := make(map[int]bool)
	for _, part := range strings.Split(spec, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		if strings.Contains(part, "-") {
			bounds := strings.SplitN(part, "-", 2)
			start, err := strconv.Atoi(strings.TrimSpace(bounds[0]))
			if err != nil {
				return nil, fmt.Errorf("invalid port range %q", part)
			}
			end, err := strconv.Atoi(strings.TrimSpace(bounds[1]))
			if err != nil {
				return nil, fmt.Errorf("invalid port range %q", part)
			}
			if start < 1 || end > 65535 || start > end {
				return nil, fmt.Errorf("invalid port range %q", part)
			}
			for port := start; port <= end; port++ {
				if !seen[port] {
					seen[port] = true
					ports = append(ports, port)
				}
			}
			continue
		}
		port, err := strconv.Atoi(part)
		if err != nil || port < 1 || port > 65535 {
			return nil, fmt.Errorf("invalid port %q", part)
		}
		if !seen[port] {
			seen[port] = true
			ports = append(ports, port)
		}
	}
	if len(ports) == 0 {
		return nil, errors.New("port_range is empty")
	}
	return ports, nil
}

func firstScanAddress(ctx context.Context, host string) (string, error) {
	if ip := net.ParseIP(host); ip != nil {
		return ip.String(), nil
	}
	lookupCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	addresses, err := net.DefaultResolver.LookupIPAddr(lookupCtx, host)
	if err != nil {
		return "", err
	}
	if len(addresses) == 0 {
		return "", fmt.Errorf("no addresses for %s", host)
	}
	if len(addresses) > 4 {
		addresses = addresses[:4]
	}
	for _, address := range addresses {
		if address.IP.To4() != nil {
			return address.IP.String(), nil
		}
	}
	return addresses[0].IP.String(), nil
}

func (s *Server) toolNetInfo(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}

	hostname, err := os.Hostname()
	if err != nil {
		hostname = ""
	}
	interfaces, err := net.Interfaces()
	if err != nil {
		return errResult(codeInternal, "INTERNAL", "list interfaces: "+err.Error())
	}
	out := make([]map[string]any, 0, len(interfaces))
	for _, iface := range interfaces {
		flags := []string{}
		if iface.Flags != 0 {
			flags = strings.Split(iface.Flags.String(), "|")
		}
		ifaceAddrs, err := iface.Addrs()
		if err != nil {
			ifaceAddrs = nil
		}
		addresses := make([]map[string]any, 0, len(ifaceAddrs))
		for _, addr := range ifaceAddrs {
			ip, ipnet, ok := parseInterfaceAddr(addr.String())
			if !ok {
				continue
			}
			family := "ipv4"
			if ip.To4() == nil {
				family = "ipv6"
			}
			addresses = append(addresses, map[string]any{
				"address":  ip.String(),
				"family":   family,
				"cidr":     ipnet.String(),
				"loopback": ip.IsLoopback(),
				"private":  ip.IsPrivate(),
			})
		}
		out = append(out, map[string]any{
			"name":      iface.Name,
			"index":     iface.Index,
			"mtu":       iface.MTU,
			"flags":     flags,
			"mac":       iface.HardwareAddr.String(),
			"addresses": addresses,
		})
	}

	return okResult(hostname, map[string]any{
		"hostname":    hostname,
		"os":          runtime.GOOS,
		"arch":        runtime.GOARCH,
		"interfaces":  out,
		"dns_servers": readDNSServers(),
	})
}

// parseInterfaceAddr tolerates zone identifiers such as fe80::1%eth0, which
// net.ParseCIDR does not accept.
func parseInterfaceAddr(value string) (net.IP, *net.IPNet, bool) {
	hostPart := value
	if before, after, ok := strings.Cut(value, "%"); ok {
		if index := strings.IndexByte(after, '/'); index >= 0 {
			hostPart = before + after[index:]
		} else {
			hostPart = before
		}
	}
	ip, ipnet, err := net.ParseCIDR(hostPart)
	if err != nil {
		return nil, nil, false
	}
	return ip, ipnet, true
}

func readDNSServers() []string {
	servers := []string{}
	if runtime.GOOS == "windows" {
		return servers
	}
	data, err := os.ReadFile("/etc/resolv.conf")
	if err != nil {
		return servers
	}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if strings.HasPrefix(line, "#") || strings.HasPrefix(line, ";") {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) < 2 || fields[0] != "nameserver" {
			continue
		}
		servers = append(servers, fields[1])
	}
	return servers
}

func (s *Server) toolWebRequest(raw json.RawMessage) toolResult {
	var args struct {
		EnvID           string            `json:"env_id"`
		URL             string            `json:"url"`
		Method          string            `json:"method"`
		Headers         map[string]string `json:"headers"`
		BodyBase64      string            `json:"body_base64"`
		JSON            any               `json:"json"`
		Form            map[string]string `json:"form"`
		TimeoutMS       int64             `json:"timeout_ms"`
		MaxBytes        int64             `json:"max_bytes"`
		FollowRedirects *bool             `json:"follow_redirects"`
		MaxRedirects    *int              `json:"max_redirects"`
		InsecureTLS     bool              `json:"insecure_tls"`
		Proxy           string            `json:"proxy"`
		UserAgent       string            `json:"user_agent"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if _, err := s.getEnv(args.EnvID); err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	args.URL = strings.TrimSpace(args.URL)
	if args.URL == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "url is required")
	}
	parsed, err := url.Parse(args.URL)
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "invalid url: "+err.Error())
	}
	if parsed.Scheme != "http" && parsed.Scheme != "https" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"url scheme must be http or https")
	}
	if parsed.Host == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "url must include a host")
	}
	if err := s.authorizeNetHost(parsed.Hostname()); err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}

	timeout := clampMillis(args.TimeoutMS, 30000, 100, 120000)
	maxBytes := args.MaxBytes
	if maxBytes <= 0 {
		maxBytes = 64 << 10
	}
	// Embedded MCP control frames are 256 KiB. Keep the base64 body, the
	// optional UTF-8 copy and headers inside that budget; larger downloads
	// belong in the workspace via exec/curl.
	if maxBytes > 64<<10 {
		maxBytes = 64 << 10
	}
	follow := true
	if args.FollowRedirects != nil {
		follow = *args.FollowRedirects
	}
	maxRedirects := 10
	if args.MaxRedirects != nil {
		maxRedirects = *args.MaxRedirects
		if maxRedirects < 0 {
			maxRedirects = 0
		}
		if maxRedirects > 20 {
			maxRedirects = 20
		}
	}

	method := strings.TrimSpace(args.Method)
	if method == "" {
		method = http.MethodGet
	}
	method = strings.ToUpper(method)

	var body io.Reader
	contentType := ""
	switch {
	case args.JSON != nil:
		encoded, err := json.Marshal(args.JSON)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "json body: "+err.Error())
		}
		if len(encoded) > 64<<10 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"json body exceeds 64 KiB; write it to the workspace and use exec/curl")
		}
		body = bytes.NewReader(encoded)
		contentType = "application/json"
	case len(args.Form) > 0:
		values := url.Values{}
		for name, value := range args.Form {
			values.Set(name, value)
		}
		encoded := values.Encode()
		if len(encoded) > 64<<10 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"form body exceeds 64 KiB; write it to the workspace and use exec/curl")
		}
		body = strings.NewReader(encoded)
		contentType = "application/x-www-form-urlencoded"
	case args.BodyBase64 != "":
		if len(args.BodyBase64) > 90_000 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"body_base64 exceeds 64 KiB; write it to the workspace and use exec/curl")
		}
		decoded, err := base64.StdEncoding.DecodeString(args.BodyBase64)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"body_base64: "+err.Error())
		}
		if len(decoded) > 16<<20 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"body_base64 exceeds 16 MiB")
		}
		body = bytes.NewReader(decoded)
	}

	ctx, cancel := context.WithTimeout(context.Background(),
		time.Duration(timeout)*time.Millisecond)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, method, args.URL, body)
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	for name, value := range args.Headers {
		if strings.EqualFold(name, "Host") {
			req.Host = value
			continue
		}
		req.Header.Set(name, value)
	}
	if contentType != "" && req.Header.Get("Content-Type") == "" {
		req.Header.Set("Content-Type", contentType)
	}
	if args.UserAgent != "" {
		req.Header.Set("User-Agent", args.UserAgent)
	}

	transport := &http.Transport{
		TLSClientConfig:   &tls.Config{InsecureSkipVerify: args.InsecureTLS},
		ForceAttemptHTTP2: true,
	}
	if args.Proxy != "" {
		proxyURL, err := url.Parse(args.Proxy)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "invalid proxy: "+err.Error())
		}
		if proxyURL.Scheme != "http" && proxyURL.Scheme != "https" {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"proxy scheme must be http or https")
		}
		if proxyURL.Host == "" {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "proxy must include a host")
		}
		transport.Proxy = http.ProxyURL(proxyURL)
	}
	defer transport.CloseIdleConnections()

	jar, err := cookiejar.New(nil)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", "cookie jar: "+err.Error())
	}
	redirects := []map[string]any{}
	client := &http.Client{
		Transport: transport,
		Jar:       jar,
		Timeout:   time.Duration(timeout) * time.Millisecond,
		CheckRedirect: func(next *http.Request, via []*http.Request) error {
			if next.Response != nil && next.Response.Request != nil {
				redirects = append(redirects, map[string]any{
					"status":   next.Response.StatusCode,
					"url":      next.Response.Request.URL.String(),
					"location": next.Response.Header.Get("Location"),
				})
			}
			if !follow {
				return http.ErrUseLastResponse
			}
			if maxRedirects == 0 || len(via) > maxRedirects {
				return http.ErrUseLastResponse
			}
			// Do not forward caller-supplied credentials or API keys to a
			// different origin, or across an HTTPS -> HTTP downgrade.
			previous := via[len(via)-1].URL
			if next.URL.Host != previous.Host ||
				(previous.Scheme == "https" && next.URL.Scheme == "http") {
				for name := range next.Header {
					if !strings.EqualFold(name, "User-Agent") &&
						!strings.EqualFold(name, "Accept") {
						next.Header.Del(name)
					}
				}
			}
			return nil
		},
	}

	started := time.Now()
	resp, err := client.Do(req)
	if err != nil {
		return errResult(codeInternal, "REQUEST_FAILED", err.Error())
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(io.LimitReader(resp.Body, maxBytes+1))
	if err != nil {
		return errResult(codeInternal, "READ_FAILED", err.Error())
	}
	truncated := int64(len(data)) > maxBytes
	if truncated {
		data = data[:maxBytes]
	}

	headers := make([][]string, 0, len(resp.Header))
	names := make([]string, 0, len(resp.Header))
	for name := range resp.Header {
		names = append(names, name)
	}
	sort.Strings(names)
	headerBytes := 0
	headersTruncated := false
	for _, name := range names {
		for _, value := range resp.Header[name] {
			if headerBytes+len(name)+len(value) > 16<<10 {
				headersTruncated = true
				break
			}
			headers = append(headers, []string{name, value})
			headerBytes += len(name) + len(value)
		}
		if headersTruncated {
			break
		}
	}

	cookies := make([]map[string]any, 0)
	for _, cookie := range jar.Cookies(resp.Request.URL) {
		expires := ""
		if !cookie.Expires.IsZero() {
			expires = cookie.Expires.UTC().Format(time.RFC3339)
		}
		cookies = append(cookies, map[string]any{
			"name":      cookie.Name,
			"value":     cookie.Value,
			"domain":    cookie.Domain,
			"path":      cookie.Path,
			"secure":    cookie.Secure,
			"http_only": cookie.HttpOnly,
			"expires":   expires,
		})
	}

	var tlsInfo any
	if resp.TLS != nil {
		tlsInfo = map[string]any{
			"version":      tlsVersionName(resp.TLS.Version),
			"cipher_suite": tlsCipherSuite(resp.TLS.CipherSuite),
			"server_name":  resp.TLS.ServerName,
		}
	}

	result := map[string]any{
		"status":            resp.StatusCode,
		"status_text":       http.StatusText(resp.StatusCode),
		"headers":           headers,
		"headers_truncated": headersTruncated,
		"body_base64":       base64.StdEncoding.EncodeToString(data),
		"bytes":             len(data),
		"truncated":         truncated,
		"duration_ms":       time.Since(started).Milliseconds(),
		"final_url":         resp.Request.URL.String(),
		"redirects":         redirects,
		"cookies":           cookies,
		"tls":               tlsInfo,
	}
	if text, ok := validText(data); ok {
		result["body_text"] = text
	}

	return okResult(fmt.Sprintf("%d %s", resp.StatusCode, http.StatusText(resp.StatusCode)), result)
}
