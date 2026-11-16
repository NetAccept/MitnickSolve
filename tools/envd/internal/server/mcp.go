package server

import (
	"encoding/json"
	"fmt"
	"runtime"
)

type rpcRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

type rpcResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  any             `json:"result,omitempty"`
	Error   *rpcError       `json:"error,omitempty"`
}

type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

func (s *Server) dispatch(req rpcRequest) (rpcResponse, bool) {
	if len(req.ID) == 0 {
		return rpcResponse{}, false
	}

	switch req.Method {
	case "initialize":
		return rpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result: map[string]any{
				"protocolVersion": "2025-11-25",
				"serverInfo":      map[string]any{"name": "envd", "version": Version},
				"capabilities": map[string]any{
					"tools": map[string]any{},
					"envd": map[string]any{
						"protocol":      map[string]any{"min": 2, "max": 2},
						"tools_version": "0.2",
						"capabilities":  s.capabilities(),
						"profiles":      s.profiles(),
					},
				},
			},
		}, true
	case "ping":
		return rpcResponse{JSONRPC: "2.0", ID: req.ID, Result: map[string]any{}}, true
	case "tools/list":
		return rpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result:  map[string]any{"tools": toolDefinitions(s.profiles())},
		}, true
	case "tools/call":
		var params struct {
			Name      string          `json:"name"`
			Arguments json.RawMessage `json:"arguments"`
		}
		if err := json.Unmarshal(req.Params, &params); err != nil {
			return rpcResponse{
				JSONRPC: "2.0",
				ID:      req.ID,
				Error:   &rpcError{Code: -32602, Message: "invalid params: " + err.Error()},
			}, true
		}
		return rpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result:  s.callTool(params.Name, params.Arguments),
		}, true
	default:
		return rpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Error:   &rpcError{Code: -32601, Message: fmt.Sprintf("method not found: %s", req.Method)},
		}, true
	}
}

func (s *Server) capabilities() []string {
	caps := []string{
		"transport." + s.transport,
		"env.managed",
		"exec",
		"files",
		"files.analysis",
		"binary.analysis",
		"data.transform",
		"net.dns",
		"net.tcp",
		"net.tls",
		"net.scan",
		"net.http",
		"net.info",
		"session.pipe",
		"session.pty",
	}
	if s.dataPlane {
		caps = append(caps, "transport.tcp")
	}
	if s.isolatedAvailable() {
		caps = append(caps, "sandbox.launcher")
	}
	if len(s.opts.NetAllow) > 0 {
		caps = append(caps, "net.allowlist")
	}
	if runtime.GOOS == "windows" {
		caps = append(caps, "process.job")
	} else {
		caps = append(caps, "process.group")
	}
	return caps
}

func toolDefinitions(profiles []string) []map[string]any {
	stringProp := func() map[string]any { return map[string]any{"type": "string"} }
	intProp := func() map[string]any { return map[string]any{"type": "integer"} }
	boolProp := func() map[string]any { return map[string]any{"type": "boolean"} }
	stringArrayProp := func() map[string]any {
		return map[string]any{"type": "array", "items": stringProp()}
	}
	intArrayProp := func() map[string]any {
		return map[string]any{"type": "array", "items": intProp()}
	}
	anyProp := func() map[string]any { return map[string]any{} }
	return []map[string]any{
		{
			"name":        "agent_info",
			"description": "Return envd version, profile, transport, and capabilities.",
			"inputSchema": objectSchema(map[string]any{}),
		},
		{
			"name":        "env_create",
			"description": "Create a managed workspace environment. The isolated profile requires a deployment-configured sandbox launcher.",
			"inputSchema": objectSchema(map[string]any{
				"name":    stringProp(),
				"kind":    map[string]any{"type": "string", "enum": []string{"managed"}},
				"profile": map[string]any{"type": "string", "enum": profiles},
				"root":    stringProp(),
			}),
		},
		{
			"name":        "env_open",
			"description": "Open an existing managed environment by id.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
			}, "env_id"),
		},
		{
			"name":        "exec",
			"description": "Run a command in an environment and return captured output.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":           stringProp(),
				"argv":             map[string]any{"type": "array", "items": stringProp()},
				"cwd":              stringProp(),
				"timeout_ms":       intProp(),
				"max_output_bytes": intProp(),
			}, "env_id", "argv"),
		},
		{
			"name":        "read_file",
			"description": "Read a file under an environment workspace. Data is base64.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
				"offset": intProp(),
				"limit":  intProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "write_file",
			"description": "Write a file under an environment workspace. Data is base64.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
				"data":   stringProp(),
				"atomic": map[string]any{"type": "boolean"},
			}, "env_id", "path", "data"),
		},
		{
			"name":        "list_dir",
			"description": "List a directory under an environment workspace.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "stat_file",
			"description": "Return metadata for a path under an environment workspace.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "session_create",
			"description": "Start a persistent shell session in an environment.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"shell":  stringProp(),
				"pty":    map[string]any{"type": "boolean"},
				"cols":   intProp(),
				"rows":   intProp(),
			}, "env_id"),
		},
		{
			"name":        "session_exec",
			"description": "Run a command in a pipe-backed persistent shell session and return its output.",
			"inputSchema": objectSchema(map[string]any{
				"session_id":       stringProp(),
				"command":          stringProp(),
				"timeout_ms":       intProp(),
				"max_output_bytes": intProp(),
			}, "session_id", "command"),
		},
		{
			"name":        "session_write",
			"description": "Write base64-encoded bytes to a session, typically a PTY.",
			"inputSchema": objectSchema(map[string]any{
				"session_id":  stringProp(),
				"data_base64": stringProp(),
			}, "session_id", "data_base64"),
		},
		{
			"name":        "session_read",
			"description": "Read available session output as base64 bytes.",
			"inputSchema": objectSchema(map[string]any{
				"session_id": stringProp(),
				"max_bytes":  intProp(),
				"timeout_ms": intProp(),
			}, "session_id"),
		},
		{
			"name":        "session_resize",
			"description": "Resize a PTY session.",
			"inputSchema": objectSchema(map[string]any{
				"session_id": stringProp(),
				"cols":       intProp(),
				"rows":       intProp(),
			}, "session_id", "cols", "rows"),
		},
		{
			"name":        "session_list",
			"description": "List persistent sessions, optionally filtered by environment.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
			}),
		},
		{
			"name":        "session_close",
			"description": "Close a persistent shell session.",
			"inputSchema": objectSchema(map[string]any{
				"session_id": stringProp(),
			}, "session_id"),
		},
		{
			"name":        "net_info",
			"description": "Return hostname, OS/arch, interfaces, addresses and DNS servers for the environment host.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
			}, "env_id"),
		},
		{
			"name":        "net_dns",
			"description": "Resolve DNS records (A, AAAA, CNAME, MX, NS, TXT, SRV, PTR) with an optional custom resolver.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":     stringProp(),
				"name":       stringProp(),
				"type":       map[string]any{"type": "string", "enum": []string{"A", "AAAA", "CNAME", "MX", "NS", "TXT", "SRV", "PTR"}},
				"server":     stringProp(),
				"timeout_ms": intProp(),
			}, "env_id", "name"),
		},
		{
			"name":        "net_dial",
			"description": "Connect TCP or TLS, optionally send bytes and read a banner. Refused connections return connected=false.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":       stringProp(),
				"host":         stringProp(),
				"port":         intProp(),
				"send_base64":  stringProp(),
				"read_bytes":   intProp(),
				"timeout_ms":   intProp(),
				"tls":          boolProp(),
				"server_name":  stringProp(),
				"insecure_tls": boolProp(),
				"alpn":         stringArrayProp(),
			}, "env_id", "host", "port"),
		},
		{
			"name":        "net_tls",
			"description": "Inspect a TLS endpoint: negotiated version/cipher/ALPN and the full peer certificate chain.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":      stringProp(),
				"host":        stringProp(),
				"port":        intProp(),
				"server_name": stringProp(),
				"timeout_ms":  intProp(),
				"alpn":        stringArrayProp(),
			}, "env_id", "host"),
		},
		{
			"name":        "net_scan",
			"description": "Bounded TCP connect scan of explicit hosts and ports with optional banner capture. Max 8192 probes per call.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":      stringProp(),
				"targets":     stringArrayProp(),
				"ports":       intArrayProp(),
				"port_range":  stringProp(),
				"timeout_ms":  intProp(),
				"concurrency": intProp(),
				"banner":      boolProp(),
			}, "env_id", "targets"),
		},
		{
			"name":        "web_request",
			"description": "Environment-scoped HTTP(S) request with methods, headers, JSON/form bodies, cookie jar, redirect tracing, proxy and TLS options.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":           stringProp(),
				"url":              stringProp(),
				"method":           stringProp(),
				"headers":          map[string]any{"type": "object", "additionalProperties": stringProp()},
				"body_base64":      stringProp(),
				"json":             anyProp(),
				"form":             map[string]any{"type": "object", "additionalProperties": stringProp()},
				"timeout_ms":       intProp(),
				"max_bytes":        intProp(),
				"follow_redirects": boolProp(),
				"max_redirects":    intProp(),
				"insecure_tls":     boolProp(),
				"proxy":            stringProp(),
				"user_agent":       stringProp(),
			}, "env_id", "url"),
		},
		{
			"name":        "file_hash",
			"description": "Hash a workspace file with md5, sha1, sha256 and/or sha512 using streaming I/O.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":     stringProp(),
				"path":       stringProp(),
				"algorithms": stringArrayProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "file_type",
			"description": "Identify a workspace file from magic bytes: ELF, PE, Mach-O, archives, documents, images, SQLite, WASM, text.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "file_strings",
			"description": "Extract printable ASCII and UTF-16LE strings with offsets from a file.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":   stringProp(),
				"path":     stringProp(),
				"min_len":  intProp(),
				"encoding": map[string]any{"type": "string", "enum": []string{"ascii", "utf16le", "both"}},
				"limit":    intProp(),
				"offset":   intProp(),
				"length":   intProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "file_hexdump",
			"description": "Hex and ASCII dump of a byte range in a workspace file.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
				"offset": intProp(),
				"length": intProp(),
				"width":  intProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "file_entropy",
			"description": "Entropy analysis with per-chunk values to spot packed, compressed or encrypted data.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":     stringProp(),
				"path":       stringProp(),
				"offset":     intProp(),
				"length":     intProp(),
				"chunk_size": intProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "file_search",
			"description": "Search workspace file names or contents (literal or regex) with bounded walking and file reads.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":           stringProp(),
				"root":             stringProp(),
				"pattern":          stringProp(),
				"mode":             map[string]any{"type": "string", "enum": []string{"name", "content"}},
				"glob":             stringProp(),
				"case_sensitive":   boolProp(),
				"regex":            boolProp(),
				"max_results":      intProp(),
				"max_file_bytes":   intProp(),
				"include_hidden":   boolProp(),
				"max_files_walked": intProp(),
			}, "env_id", "pattern"),
		},
		{
			"name":        "archive_list",
			"description": "List entries in a zip, tar, tar.gz or gzip archive under the workspace.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "archive_extract",
			"description": "Safely extract a zip/tar/tar.gz/gz archive into the workspace with traversal and size protection.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":          stringProp(),
				"path":            stringProp(),
				"dest":            stringProp(),
				"overwrite":       boolProp(),
				"max_entries":     intProp(),
				"max_total_bytes": intProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "binary_info",
			"description": "Inspect ELF, PE or Mach-O binaries: architecture, sections, imports and exploit mitigations.",
			"inputSchema": objectSchema(map[string]any{
				"env_id": stringProp(),
				"path":   stringProp(),
			}, "env_id", "path"),
		},
		{
			"name":        "secret_scan",
			"description": "Scan workspace text files for keys, tokens, JWTs, private key blocks and connection strings. Previews are redacted.",
			"inputSchema": objectSchema(map[string]any{
				"env_id":         stringProp(),
				"root":           stringProp(),
				"glob":           stringProp(),
				"max_files":      intProp(),
				"max_file_bytes": intProp(),
				"include_hidden": boolProp(),
			}, "env_id"),
		},
		{
			"name":        "transform",
			"description": "CTF data transforms: base64/base64url, hex, URL, HTML, hashes, HMAC, JWT decode, gzip, ROT13, XOR, UUID, random bytes.",
			"inputSchema": objectSchema(map[string]any{
				"op":          map[string]any{"type": "string", "enum": []string{"base64_encode", "base64_decode", "base64url_encode", "base64url_decode", "hex_encode", "hex_decode", "url_encode", "url_decode", "html_escape", "html_unescape", "hash", "hmac", "jwt_decode", "gzip", "gunzip", "rot13", "xor", "uuid", "random_bytes"}},
				"data":        stringProp(),
				"data_base64": stringProp(),
				"key":         stringProp(),
				"key_base64":  stringProp(),
				"algorithm":   stringProp(),
				"algorithms":  stringArrayProp(),
				"length":      intProp(),
				"xor_key":     stringProp(),
			}, "op"),
		},
	}
}

func objectSchema(properties map[string]any, required ...string) map[string]any {
	schema := map[string]any{
		"type":                 "object",
		"properties":           properties,
		"additionalProperties": false,
	}
	if len(required) > 0 {
		schema["required"] = required
	}
	return schema
}
