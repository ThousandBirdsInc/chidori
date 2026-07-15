"""Unit tests for the AgentClient HTTP layer: typed errors, timeout, retry.

These run against in-process stdlib servers — no chidori binary needed —
so they complement the end-to-end coverage in test_session_api.py.

Run:  python -m unittest sdk/python/tests/test_client_http.py
"""

from __future__ import annotations

import http.server
import json
import os
import socketserver
import sys
import threading
import time
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from chidori import (  # noqa: E402
    AgentClient,
    AgentClientError,
    ConnectionError,
    HttpError,
    TimeoutError,
)


class _Handler(http.server.BaseHTTPRequestHandler):
    """Scriptable handler: the test case sets `script`, a list of
    (status, body_dict) responses consumed in order (the last repeats)."""

    script: list[tuple[int, dict]] = [(200, {"status": "ok"})]
    calls: int = 0

    def _respond(self) -> None:
        idx = min(_Handler.calls, len(_Handler.script) - 1)
        _Handler.calls += 1
        status, payload = _Handler.script[idx]
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - stdlib naming
        self._respond()

    def do_POST(self) -> None:  # noqa: N802 - stdlib naming
        self._respond()

    def log_message(self, *args: object) -> None:
        pass


class HttpLayerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.base_url = f"http://127.0.0.1:{cls.server.server_address[1]}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        _Handler.script = [(200, {"status": "ok"})]
        _Handler.calls = 0
        self.client = AgentClient(self.base_url, retries=2, retry_delay_seconds=0.01)

    def test_http_error_carries_status_and_detail(self) -> None:
        _Handler.script = [(409, {"error": "run is terminal"})]
        with self.assertRaises(HttpError) as ctx:
            self.client.resume("x", "yes")
        err = ctx.exception
        self.assertEqual(err.status, 409)
        self.assertEqual(err.detail, "run is terminal")
        self.assertIn("409", str(err))
        # Still a RuntimeError, for pre-existing handlers.
        self.assertIsInstance(err, AgentClientError)
        self.assertIsInstance(err, RuntimeError)

    def test_distinct_statuses_are_distinguishable(self) -> None:
        for status in (400, 404, 409):
            _Handler.script = [(status, {"error": f"status {status}"})]
            _Handler.calls = 0
            with self.assertRaises(HttpError) as ctx:
                self.client.signal("x", "review")
            self.assertEqual(ctx.exception.status, status)

    def test_get_retries_on_retryable_status(self) -> None:
        _Handler.script = [(503, {"error": "warming up"}), (503, {"error": "warming up"}), (200, {"status": "ok"})]
        self.assertEqual(self.client.health(), {"status": "ok"})
        self.assertEqual(_Handler.calls, 3)

    def test_get_gives_up_after_retries(self) -> None:
        _Handler.script = [(503, {"error": "down"})]
        client = AgentClient(self.base_url, retries=1, retry_delay_seconds=0.01)
        with self.assertRaises(HttpError) as ctx:
            client.health()
        self.assertEqual(ctx.exception.status, 503)
        self.assertEqual(_Handler.calls, 2)

    def test_get_does_not_retry_non_retryable_status(self) -> None:
        _Handler.script = [(404, {"error": "no such session"})]
        with self.assertRaises(HttpError) as ctx:
            self.client.get_session("nope")
        self.assertEqual(ctx.exception.status, 404)
        self.assertEqual(_Handler.calls, 1)

    def test_post_is_never_retried(self) -> None:
        _Handler.script = [(503, {"error": "overloaded"})]
        with self.assertRaises(HttpError) as ctx:
            self.client.run({"q": 1})
        self.assertEqual(ctx.exception.status, 503)
        self.assertEqual(_Handler.calls, 1)

    def test_connection_error_when_nothing_listens(self) -> None:
        client = AgentClient("http://127.0.0.1:1", retries=0, timeout_seconds=2)
        with self.assertRaises(ConnectionError):
            client.health()

    def test_timeout_on_silent_server(self) -> None:
        class Silent(socketserver.BaseRequestHandler):
            def handle(self) -> None:
                time.sleep(5)

        silent = socketserver.TCPServer(("127.0.0.1", 0), Silent)
        threading.Thread(target=silent.serve_forever, daemon=True).start()
        try:
            port = silent.server_address[1]
            client = AgentClient(f"http://127.0.0.1:{port}", retries=0, timeout_seconds=0.3)
            with self.assertRaises(TimeoutError):
                client.run({"q": 1})
        finally:
            silent.shutdown()
            silent.server_close()


class _SseHandler(http.server.BaseHTTPRequestHandler):
    """Emits a fixed SSE stream for `POST /sessions/stream`: one of every
    event kind the server produces, so the parser's pass-through is pinned."""

    def do_POST(self) -> None:  # noqa: N802 - stdlib naming
        frames = [
            ("call", {"seq": 1, "function": "prompt"}),
            ("prompt_delta", {"label": "draft", "delta": "hi"}),
            ("paused", {"id": "s-1", "status": "paused", "pending_seq": 2}),
            ("done", {"id": "s-1", "status": "completed", "output": {"ok": True}}),
        ]
        body = "".join(
            f"event: {name}\ndata: {json.dumps(payload)}\n\n" for name, payload in frames
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args: object) -> None:
        pass


class StreamParserTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = socketserver.TCPServer(("127.0.0.1", 0), _SseHandler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        port = cls.server.server_address[1]
        cls.client = AgentClient(f"http://127.0.0.1:{port}", retries=0, timeout_seconds=5)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def test_stream_yields_every_event_kind_including_paused(self) -> None:
        events = list(self.client.stream({"q": 1}))
        self.assertEqual(
            [e["type"] for e in events],
            ["call", "prompt_delta", "paused", "done"],
        )
        paused = events[2]
        self.assertEqual(paused["status"], "paused")
        self.assertEqual(paused["pending_seq"], 2)
        self.assertEqual(events[3]["status"], "completed")


if __name__ == "__main__":
    unittest.main()
