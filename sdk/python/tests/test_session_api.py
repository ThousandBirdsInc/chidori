"""Integration tests for the chidori session API.

These tests drive a real `chidori serve` subprocess through the Python
SDK, pointing it at a stdlib-only mock LLM server so no real network calls
happen. They lock in the contract for the recently-hardened session API:
concurrency limits + 503, bearer-token auth, CORS headers, pause/resume,
and byte-identical replay from a checkpoint.

Run with:
    cd sdk/python && python3 -m unittest tests/test_session_api.py

Set `CHIDORI_BIN` to override the binary path if it isn't at the default
`target/debug/chidori` relative to the repo root.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

# The SDK lives alongside this test file.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from chidori import AgentClient, Checkpoint  # noqa: E402

# ---------------------------------------------------------------------------
# Paths + defaults
# ---------------------------------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parents[3]
AGENT_BIN = Path(
    os.environ.get("CHIDORI_BIN", str(REPO_ROOT / "target" / "debug" / "chidori"))
)
FIXTURES = Path(__file__).parent / "fixtures"


# ---------------------------------------------------------------------------
# Mock LLM server — looks enough like a LiteLLM / OpenAI chat endpoint that
# the OpenAiProvider catch-all in `providers/openai.rs` accepts its output.
# ---------------------------------------------------------------------------


class MockLlm:
    """Minimal HTTP server that returns a canned chat completion.

    Tracks how many times `/v1/chat/completions` was hit so tests can
    assert "this call was a replay" (hits stayed flat) or "this call
    talked to the LLM" (hits went up).
    """

    def __init__(self, response_text: str = "forty-two"):
        self.response_text = response_text
        self.hits = 0
        self._server: HTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.port: int = 0

    def start(self) -> None:
        mock = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):  # silence stdout
                pass

            def do_POST(self):
                mock.hits += 1
                length = int(self.headers.get("content-length", "0"))
                _ = self.rfile.read(length)
                body = {
                    "id": "mock-1",
                    "object": "chat.completion",
                    "model": "mock-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": mock.response_text,
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 5,
                        "completion_tokens": 3,
                        "total_tokens": 8,
                    },
                }
                payload = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._server = HTTPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._thread = None
            self._server = None


# ---------------------------------------------------------------------------
# App-agent subprocess harness
# ---------------------------------------------------------------------------


def _free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _wait_for_health(base_url: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(base_url + "/health", timeout=1) as resp:
                if resp.status == 200:
                    return
        except (urllib.error.URLError, ConnectionError, OSError):
            pass
        time.sleep(0.1)
    raise RuntimeError(f"chidori never came up at {base_url}")


class ServeProcess:
    """Start `chidori serve` as a subprocess against a given agent file.

    Takes extra env vars for hardening/test configuration. Cleans up on
    `stop()` via SIGTERM + timeout-bounded wait.
    """

    def __init__(self, agent: Path, extra_env: dict[str, str] | None = None):
        self.agent = agent
        self.extra_env = extra_env or {}
        self.port = _free_port()
        self.proc: subprocess.Popen | None = None
        self.base_url = f"http://127.0.0.1:{self.port}"

    def start(self) -> None:
        env = os.environ.copy()
        # Wipe any real provider credentials the developer has set — we
        # route everything through the mock via LITELLM_API_URL.
        for key in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY"):
            env.pop(key, None)
        env.update(self.extra_env)
        if not AGENT_BIN.exists():
            raise RuntimeError(
                f"chidori binary not found at {AGENT_BIN}; "
                "run `cargo build` or set CHIDORI_BIN"
            )
        self.proc = subprocess.Popen(
            [str(AGENT_BIN), "serve", str(self.agent), "--port", str(self.port)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        _wait_for_health(self.base_url)

    def stop(self) -> None:
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=2)
            self.proc = None


# ---------------------------------------------------------------------------
# Base class: default server with a mock LLM wired up via LITELLM_API_URL
# ---------------------------------------------------------------------------


class _MockLlmTestCase(unittest.TestCase):
    """Shared harness: mock LLM + chidori + Python SDK client.

    Subclasses override `extra_env` to configure auth, concurrency, CORS.
    Each test class gets its own subprocess so env-var changes take
    effect; each individual test shares that subprocess.
    """

    agent: Path = FIXTURES / "ask.star"
    extra_env: dict[str, str] = {}

    @classmethod
    def setUpClass(cls) -> None:
        cls.mock = MockLlm(response_text="forty-two")
        cls.mock.start()
        env = dict(cls.extra_env)
        env.setdefault("LITELLM_API_URL", f"http://127.0.0.1:{cls.mock.port}/v1")
        env.setdefault("LITELLM_API_KEY", "test-key")
        cls.serve = ServeProcess(cls.agent, env)
        cls.serve.start()
        cls.client = AgentClient(cls.serve.base_url)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.serve.stop()
        cls.mock.stop()


# ---------------------------------------------------------------------------
# Core session API: run, checkpoint, replay
# ---------------------------------------------------------------------------


class SessionApiTests(_MockLlmTestCase):
    def test_health(self):
        with urllib.request.urlopen(self.serve.base_url + "/health") as r:
            body = json.loads(r.read())
        self.assertEqual(body, {"status": "ok"})

    def test_run_returns_output_with_mock_content(self):
        self.mock.hits = 0
        session = self.client.run({"question": "what is the answer"})
        self.assertEqual(session.status, "completed")
        self.assertIsNotNone(session.output)
        self.assertEqual(session.output["answer"], "forty-two")
        self.assertEqual(self.mock.hits, 1)

    def test_checkpoint_has_call_log(self):
        session = self.client.run({"question": "q"})
        checkpoint = session.checkpoint()
        self.assertEqual(checkpoint.session_id, session.id)
        self.assertGreaterEqual(len(checkpoint.call_log), 1)
        self.assertEqual(checkpoint.call_log[0]["function"], "prompt")

    def test_replay_is_deterministic_without_hitting_llm(self):
        session = self.client.run({"question": "q"})
        cp = session.checkpoint()
        hits_before = self.mock.hits
        replayed = self.client.replay(cp)
        self.assertEqual(replayed.status, "completed")
        self.assertEqual(replayed.output, session.output)
        # Replay must not call the upstream LLM — the whole point of
        # checkpoints. If the mock's hit count went up, replay isn't
        # actually caching.
        self.assertEqual(self.mock.hits, hits_before)

    def test_list_sessions_includes_runs(self):
        # Make sure at least one session exists.
        self.client.run({"question": "list me"})
        sessions = self.client.list_sessions()
        self.assertGreater(len(sessions), 0)
        self.assertIn("id", sessions[0])
        self.assertIn("status", sessions[0])

    def test_get_checkpoint_by_id(self):
        session = self.client.run({"question": "checkpoint me"})
        checkpoint = self.client.get_checkpoint(session.id)
        self.assertEqual(checkpoint.session_id, session.id)
        self.assertGreaterEqual(len(checkpoint.call_log), 1)
        self.assertEqual(checkpoint.call_log[0]["function"], "prompt")

    def test_stream_emits_call_then_done(self):
        events = list(self.client.stream({"question": "stream me"}))
        # At least one `call` event (the prompt()) and exactly one `done`.
        call_events = [e for e in events if e["type"] == "call"]
        done_events = [e for e in events if e["type"] == "done"]
        self.assertGreaterEqual(len(call_events), 1)
        self.assertEqual(len(done_events), 1)

        # Call events carry a record with a seq and a function name.
        first_call = call_events[0]["record"]
        self.assertEqual(first_call["function"], "prompt")
        self.assertEqual(first_call["seq"], 1)

        # Done event carries the final output.
        done = done_events[0]
        self.assertEqual(done["status"], "completed")
        self.assertEqual(done["output"]["answer"], "forty-two")


# ---------------------------------------------------------------------------
# Pause / resume via input()
# ---------------------------------------------------------------------------


class PauseResumeTests(_MockLlmTestCase):
    agent = FIXTURES / "approval.star"

    def test_session_pauses_and_resumes(self):
        paused = self.client.run({"action": "delete-prod-db"})
        self.assertEqual(paused.status, "paused")
        self.assertIsNotNone(paused.pending_prompt)
        self.assertIn("delete-prod-db", paused.pending_prompt)

        resumed = self.client.resume(paused.id, "yes")
        self.assertEqual(resumed.status, "completed")
        self.assertEqual(resumed.output["action"], "delete-prod-db")
        self.assertTrue(resumed.output["approved"])

    def test_resume_with_negative_response(self):
        paused = self.client.run({"action": "drop-table"})
        resumed = self.client.resume(paused.id, "no thanks")
        self.assertEqual(resumed.status, "completed")
        self.assertFalse(resumed.output["approved"])


# ---------------------------------------------------------------------------
# Auth middleware
# ---------------------------------------------------------------------------


class AuthTests(_MockLlmTestCase):
    extra_env = {"CHIDORI_API_KEY": "test-bearer-token"}

    def _post_raw(self, path: str, body: dict, headers: dict | None = None):
        req = urllib.request.Request(
            self.serve.base_url + path,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json", **(headers or {})},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req) as resp:
                return resp.status, json.loads(resp.read())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"{}")

    def test_health_stays_open(self):
        with urllib.request.urlopen(self.serve.base_url + "/health") as r:
            self.assertEqual(r.status, 200)

    def test_missing_token_is_401(self):
        status, body = self._post_raw("/sessions", {"input": {"question": "q"}})
        self.assertEqual(status, 401)
        self.assertIn("bearer", body.get("error", "").lower())

    def test_wrong_token_is_401(self):
        status, _ = self._post_raw(
            "/sessions",
            {"input": {"question": "q"}},
            headers={"Authorization": "Bearer wrong-token"},
        )
        self.assertEqual(status, 401)

    def test_correct_token_succeeds(self):
        status, body = self._post_raw(
            "/sessions",
            {"input": {"question": "q"}},
            headers={"Authorization": "Bearer test-bearer-token"},
        )
        self.assertEqual(status, 201)
        self.assertEqual(body["status"], "completed")


# ---------------------------------------------------------------------------
# Concurrency semaphore + 503 saturation
# ---------------------------------------------------------------------------


class ConcurrencyTests(_MockLlmTestCase):
    agent = FIXTURES / "slow.star"
    extra_env = {
        "CHIDORI_MAX_CONCURRENT_SESSIONS": "1",
        "CHIDORI_ACQUIRE_TIMEOUT_MS": "200",
        "CHIDORI_SHELL_ALLOW": "sleep",
    }

    def test_second_concurrent_request_gets_503(self):
        # Kick off one request that will hold the semaphore for ~1s.
        def hold():
            try:
                self.client.run({"label": "first"})
            except Exception:
                pass

        holder = threading.Thread(target=hold, daemon=True)
        holder.start()
        # Give the holder a head start so it owns the one available permit.
        time.sleep(0.15)

        req = urllib.request.Request(
            self.serve.base_url + "/sessions",
            data=json.dumps({"input": {"label": "second"}}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req) as resp:
                self.fail(f"expected 503, got {resp.status}")
        except urllib.error.HTTPError as e:
            self.assertEqual(e.code, 503)
            body = json.loads(e.read())
            self.assertIn("busy", body["error"])
            self.assertEqual(body["acquire_timeout_ms"], 200)
        holder.join(timeout=5)


# ---------------------------------------------------------------------------
# CORS layer
# ---------------------------------------------------------------------------


class CorsTests(_MockLlmTestCase):
    extra_env = {"CHIDORI_CORS_ORIGINS": "*"}

    def test_preflight_emits_allow_origin(self):
        req = urllib.request.Request(
            self.serve.base_url + "/sessions",
            method="OPTIONS",
            headers={
                "Origin": "https://example.com",
                "Access-Control-Request-Method": "POST",
            },
        )
        with urllib.request.urlopen(req) as resp:
            self.assertEqual(resp.status, 200)
            self.assertEqual(
                resp.headers.get("access-control-allow-origin"), "*"
            )
            # The methods header is an asterisk when we configured Any.
            self.assertIn("*", resp.headers.get("access-control-allow-methods", ""))


if __name__ == "__main__":
    unittest.main()
