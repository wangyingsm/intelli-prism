#!/usr/bin/env python3
"""A model server that answers one finished message, for the api suite's agent runs.

The suite's own server needs an upstream before a run has anywhere to send a turn, and a
run is the only endpoint in the suite that reaches one. This answers in the anthropic
family, says a fixed sentence and stops, so a run over it takes exactly one turn.

    stub_upstream.py <port>
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

ANSWER = {
    "id": "msg_stub",
    "model": "claude-opus-5",
    "role": "assistant",
    "type": "message",
    "stop_reason": "end_turn",
    "content": [{"type": "text", "text": "four crates are left"}],
    "usage": {"input_tokens": 11, "output_tokens": 5},
}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 - the name the base class dispatches on
        self.rfile.read(int(self.headers.get("content-length", 0)))
        body = json.dumps(ANSWER).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
