"""Minimal OpenAI-compatible mock for local provider tests.

First POST returns an `echo` tool call; subsequent POSTs return a final text
response. The kernel should execute the tool and then finish the loop.
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    calls = 0

    def do_GET(self):
        body = json.dumps({"ok": True, "path": self.path}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            request = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            request = {}
        if "slow" in json.dumps(request):
            time.sleep(3)
        Handler.calls += 1
        if request.get("stream") and not self.path.endswith("/responses"):
            self.send_chat_stream(request)
            return
        if self.path.endswith("/responses"):
            payload = self.responses_payload()
        else:
            payload = self.chat_payload(request)

        data = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_chat_stream(self, request):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if Handler.calls == 1 and self.wants_exec(request):
            args = {
                "env_id": "$ENV",
                "argv": (
                    ["cmd", "/C", "echo approved"]
                    if sys.platform == "win32"
                    else ["sh", "-c", "echo approved"]
                ),
            }
            events = [
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_exec",
                                        "function": {
                                            "name": "exec",
                                            "arguments": json.dumps(args),
                                        },
                                    }
                                ]
                            }
                        }
                    ]
                },
                {"choices": [], "usage": {"total_tokens": 7}},
            ]
        elif Handler.calls == 1:
            events = [
                {"choices": [{"delta": {"content": "mock "}}]},
                {"choices": [{"delta": {"content": "stream"}}]},
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_1",
                                        "function": {
                                            "name": "echo",
                                            "arguments": json.dumps(
                                                {"message": "hello from sse"}
                                            ),
                                        },
                                    }
                                ]
                            }
                        }
                    ]
                },
                {"choices": [], "usage": {"total_tokens": 7}},
            ]
        else:
            events = [
                {"choices": [{"delta": {"content": "sse complete"}}]},
                {"choices": [], "usage": {"total_tokens": 5}},
            ]
        for event in events:
            self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")

    def wants_exec(self, request):
        for message in request.get("messages", []):
            if message.get("role") == "user" and "exec" in (message.get("content") or ""):
                return True
        return False

    def wants_file(self, request):
        for message in request.get("messages", []):
            if message.get("role") == "user" and "file" in (message.get("content") or ""):
                return True
        return False

    def wants_http(self, request):
        for message in request.get("messages", []):
            if message.get("role") == "user" and "http" in (message.get("content") or ""):
                return True
        return False

    def wants_spawn(self, request):
        for message in request.get("messages", []):
            if message.get("role") == "user" and "spawn" in (message.get("content") or ""):
                return True
        return False

    def spawn_context(self, request):
        task_id = None
        has_result = False
        for message in request.get("messages", []):
            if message.get("role") != "tool":
                continue
            try:
                value = json.loads(message.get("content") or "{}")
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict) and "task_id" in value:
                task_id = value["task_id"]
            if isinstance(value, dict) and "transcript" in value:
                has_result = True
        return task_id, has_result

    def chat_payload(self, request):
        wants_file = self.wants_file(request)
        wants_spawn = self.wants_spawn(request)
        if wants_spawn and Handler.calls > 1:
            task_id, has_result = self.spawn_context(request)
            if task_id and not has_result:
                return {
                    "choices": [
                        {
                            "message": {
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "call_agent_result",
                                        "type": "function",
                                        "function": {
                                            "name": "agent_result",
                                            "arguments": json.dumps(
                                                {
                                                    "task_id": task_id,
                                                    "wait_ms": 10000,
                                                }
                                            ),
                                        },
                                    }
                                ],
                            }
                        }
                    ],
                    "usage": {"total_tokens": 7},
                }
            return {
                "choices": [
                    {"message": {"content": "spawn complete", "tool_calls": []}}
                ],
                "usage": {"total_tokens": 5},
            }
        if Handler.calls == 1 and wants_spawn:
            return {
                "choices": [
                    {
                        "message": {
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "call_spawn",
                                    "type": "function",
                                    "function": {
                                        "name": "spawn_agent",
                                        "arguments": json.dumps(
                                            {
                                                "goal": "say hello child",
                                                "max_steps": 4,
                                            }
                                        ),
                                    },
                                }
                            ],
                        }
                    }
                ],
                "usage": {"total_tokens": 7},
            }
        if Handler.calls == 1:
            if self.wants_http(request):
                return {
                    "choices": [
                        {
                            "message": {
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "call_http",
                                        "type": "function",
                                        "function": {
                                            "name": "http_request",
                                            "arguments": json.dumps(
                                                {
                                                    "url": "http://127.0.0.1:8765/healthz",
                                                    "method": "GET",
                                                }
                                            ),
                                        },
                                    }
                                ],
                            }
                        }
                    ],
                    "usage": {"total_tokens": 7},
                }
            if self.wants_exec(request):
                return {
                    "choices": [
                        {
                            "message": {
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "call_exec",
                                        "type": "function",
                                        "function": {
                                            "name": "exec",
                                            "arguments": json.dumps(
                                                {
                                                    "env_id": "$ENV",
                                                    "argv": (
                                                        ["cmd", "/C", "echo approved"]
                                                        if sys.platform == "win32"
                                                        else ["sh", "-c", "echo approved"]
                                                    ),
                                                }
                                            ),
                                        },
                                    }
                                ],
                            }
                        }
                    ],
                    "usage": {"total_tokens": 7},
                }
            if wants_file:
                return {
                    "choices": [
                        {
                            "message": {
                                "content": "",
                                "tool_calls": [
                                    {
                                        "id": "call_write",
                                        "type": "function",
                                        "function": {
                                            "name": "write_file",
                                            "arguments": json.dumps(
                                                {
                                                    "env_id": "$ENV",
                                                    "path": "note.txt",
                                                    "data": "aGVsbG8gZmlsZQ==",
                                                    "atomic": True,
                                                }
                                            ),
                                        },
                                    }
                                ],
                            }
                        }
                    ],
                    "usage": {"total_tokens": 7},
                }
            return {
                "choices": [
                    {
                        "message": {
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "echo",
                                        "arguments": json.dumps(
                                            {"message": "hello from mock"}
                                        ),
                                    },
                                }
                            ],
                        }
                    }
                ],
                "usage": {"total_tokens": 7},
            }
        if Handler.calls == 2 and wants_file:
            return {
                "choices": [
                    {
                        "message": {
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "call_read",
                                    "type": "function",
                                    "function": {
                                        "name": "read_file",
                                        "arguments": json.dumps(
                                            {
                                                "env_id": "$ENV",
                                                "path": "note.txt",
                                            }
                                        ),
                                    },
                                }
                            ],
                        }
                    }
                ],
                "usage": {"total_tokens": 7},
            }
        return {
            "choices": [
                {"message": {"content": "mock complete", "tool_calls": []}}
            ],
            "usage": {"total_tokens": 5},
        }

    def responses_payload(self):
        if Handler.calls == 1:
            return {
                "output": [
                    {
                        "type": "function_call",
                        "name": "echo",
                        "arguments": json.dumps({"message": "hello from responses"}),
                        "call_id": "call_1",
                    }
                ],
                "usage": {"total_tokens": 7},
            }
        return {
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "responses mock complete"}
                    ],
                }
            ],
            "usage": {"total_tokens": 5},
        }

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
