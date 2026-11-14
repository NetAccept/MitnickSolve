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
						"tools_version": "0.1",
						"capabilities":  s.capabilities(),
						"profiles":      []string{"dev", "host", "isolated"},
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
			Result:  map[string]any{"tools": toolDefinitions()},
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
	caps := []string{"transport.embedded-stdio", "env.managed", "exec", "files"}
	if runtime.GOOS == "windows" {
		caps = append(caps, "signals.console", "fs.acl")
	} else {
		caps = append(caps, "signals.posix", "pty.posix")
	}
	return caps
}

func toolDefinitions() []map[string]any {
	stringProp := func() map[string]any { return map[string]any{"type": "string"} }
	intProp := func() map[string]any { return map[string]any{"type": "integer"} }

	return []map[string]any{
		{
			"name":        "agent_info",
			"description": "Return envd version, profile, transport, and capabilities.",
			"inputSchema": objectSchema(map[string]any{}),
		},
		{
			"name":        "env_create",
			"description": "Create a managed workspace environment.",
			"inputSchema": objectSchema(map[string]any{
				"name":    stringProp(),
				"kind":    map[string]any{"type": "string", "enum": []string{"managed"}},
				"profile": map[string]any{"type": "string", "enum": []string{"dev", "host", "isolated"}},
				"root":    stringProp(),
			}),
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
