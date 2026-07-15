#!/usr/bin/env python3
"""Minimal OpenAI-compatible chat-completions server for exercising Chidori's
LITELLM provider path without a real API key.

Responses are deterministic (a SHA-256 digest of the last message), and the
server keeps a request counter exposed at GET /__count — which is how
run_experiments.sh proves that replay makes exactly zero provider calls.
"""
import json
import hashlib
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler

COUNT = 0
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send_json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/__count":
            self._send_json({"count": COUNT})
        else:
            self._send_json({"error": "not found"}, 404)

    def do_POST(self):
        global COUNT
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")
        with LOCK:
            COUNT += 1
        last = ""
        for m in req.get("messages", []):
            content = m.get("content")
            if isinstance(content, list):
                content = " ".join(
                    p.get("text", "") for p in content if isinstance(p, dict)
                )
            last = content or last
        digest = hashlib.sha256((last or "").encode()).hexdigest()[:8]
        text = f"FAKE-LLM-RESPONSE[{digest}] to: {(last or '')[:60]}"
        self._send_json({
            "id": "chatcmpl-fake",
            "object": "chat.completion",
            "created": 0,
            "model": req.get("model", "fake"),
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": text},
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 4401), Handler).serve_forever()
