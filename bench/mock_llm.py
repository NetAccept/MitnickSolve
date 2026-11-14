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
            self.send_chat_stream()
            return
        if self.path.endswith("/responses"):
            payload = self.responses_payload()
        else:
            payload = self.chat_payload()

        data = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_chat_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if Handler.calls == 1:
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

    def chat_payload(self):
        if Handler.calls == 1:
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
