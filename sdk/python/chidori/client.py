"""Python SDK client for the chidori server."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

import urllib.request
import urllib.error


@dataclass
class Checkpoint:
    """A saved execution checkpoint that can be used to replay a session.

    Contains the session ID, input, and the full call log — enough
    to reconstruct execution without re-running LLM calls.
    """

    session_id: str
    input: dict
    call_log: list[dict]

    def save(self, path: str | Path) -> None:
        """Save checkpoint to a JSON file."""
        data = {
            "session_id": self.session_id,
            "input": self.input,
            "call_log": self.call_log,
        }
        Path(path).write_text(json.dumps(data, indent=2))

    @classmethod
    def load(cls, path: str | Path) -> Checkpoint:
        """Load checkpoint from a JSON file."""
        data = json.loads(Path(path).read_text())
        return cls(
            session_id=data["session_id"],
            input=data["input"],
            call_log=data["call_log"],
        )


@dataclass
class Session:
    """A single agent execution session with its result and call log."""

    id: str
    status: str
    input: dict
    output: Any | None = None
    error: str | None = None
    call_log: list[dict] = field(default_factory=list)
    # Populated when status == "paused" — the seq the agent is waiting on
    # and the prompt it passed to `input()`, so the client can surface it
    # to the human and later call `client.resume(session.id, response)`.
    pending_seq: int | None = None
    pending_prompt: str | None = None
    _client: AgentClient | None = field(default=None, repr=False)

    @property
    def ok(self) -> bool:
        return self.status == "completed"

    def checkpoint(self) -> Checkpoint:
        """Get the checkpoint for this session.

        If the call log wasn't fetched yet, fetches it from the server.
        """
        if not self.call_log and self._client:
            data = self._client._get(f"/sessions/{self.id}/checkpoint")
            self.call_log = data.get("call_log", [])
        return Checkpoint(
            session_id=self.id,
            input=self.input,
            call_log=self.call_log,
        )

    def replay(self) -> Session:
        """Replay this session from its checkpoint.

        Returns a new session that fast-forwards through cached LLM calls
        and picks up live execution where the checkpoint ends.
        """
        if not self._client:
            raise RuntimeError("Session not connected to a client")
        return self._client.replay(self.checkpoint())


class AgentClient:
    """Client for the chidori server.

    Manages sessions — each session is an independent agent execution
    with its own inputs, outputs, and call log (checkpoint).

    Example:
        client = AgentClient("http://localhost:8080")

        # Create and run a session
        s1 = client.run({"question": "What is Rust?"})
        print(s1.output)

        # Save checkpoint
        cp = s1.checkpoint()
        cp.save("checkpoint.json")

        # Later: replay from checkpoint (no LLM calls)
        cp2 = Checkpoint.load("checkpoint.json")
        s2 = client.replay(cp2)
        assert s1.output == s2.output  # same result, zero LLM calls
    """

    def __init__(self, base_url: str = "http://localhost:8080"):
        self.base_url = base_url.rstrip("/")

    def health(self) -> dict:
        """Check server health."""
        return self._get("/health")

    def run(self, input: dict) -> Session:
        """Create a new session and run the agent with the given input.

        Returns a Session with the output, status, and call log. If the
        agent called `input()`, the returned Session will have
        status == "paused" and a populated `pending_prompt` — use
        `client.resume(session.id, response)` to continue.
        """
        data = self._post("/sessions", {"input": input})
        return Session(
            id=data["id"],
            status=data["status"],
            input=input,
            output=data.get("output"),
            error=data.get("error"),
            pending_seq=data.get("pending_seq"),
            pending_prompt=data.get("pending_prompt"),
            _client=self,
        )

    def replay(self, checkpoint: Checkpoint) -> Session:
        """Replay an agent from a saved checkpoint.

        The runtime re-executes the Starlark code but returns cached
        results for all host function calls in the checkpoint's call log.
        No LLM calls are made for cached entries.

        If the agent code has changed since the checkpoint was saved,
        execution continues normally from the point of divergence.
        """
        data = self._post("/sessions", {
            "input": checkpoint.input,
            "replay_from": checkpoint.call_log,
        })
        return Session(
            id=data["id"],
            status=data["status"],
            input=checkpoint.input,
            output=data.get("output"),
            error=data.get("error"),
            pending_seq=data.get("pending_seq"),
            pending_prompt=data.get("pending_prompt"),
            _client=self,
        )

    def resume(self, session_id: str, response: str) -> Session:
        """Supply a response to a paused `input()` call and continue the run.

        The same session id advances to `completed` (or re-pauses on a
        subsequent `input()` call).
        """
        data = self._post(f"/sessions/{session_id}/resume", {"response": response})
        return Session(
            id=data["id"],
            status=data["status"],
            input=data.get("input", {}),
            output=data.get("output"),
            error=data.get("error"),
            pending_seq=data.get("pending_seq"),
            pending_prompt=data.get("pending_prompt"),
            _client=self,
        )

    def get_session(self, session_id: str) -> Session:
        """Get an existing session by ID."""
        data = self._get(f"/sessions/{session_id}")
        return Session(
            id=data["id"],
            status=data["status"],
            input=data.get("input", {}),
            output=data.get("output"),
            error=data.get("error"),
            pending_seq=data.get("pending_seq"),
            pending_prompt=data.get("pending_prompt"),
            _client=self,
        )

    def list_sessions(self) -> list[dict]:
        """List all sessions."""
        data = self._get("/sessions")
        return data.get("sessions", [])

    def get_checkpoint(self, session_id: str) -> Checkpoint:
        """Fetch the full call log for a session and return it as a Checkpoint.

        Equivalent to `session.checkpoint()` but works when you only have
        the id (e.g. after a server restart where the local Session handle
        is gone).
        """
        data = self._get(f"/sessions/{session_id}/checkpoint")
        return Checkpoint(
            session_id=session_id,
            input=data.get("input", {}),
            call_log=data.get("call_log", []),
        )

    def stream(self, input: dict) -> Iterator[dict]:
        """Run an agent with live per-call streaming.

        Yields a sequence of event dicts parsed from the server's
        `POST /sessions/stream` SSE endpoint. Each event has one of:

          * `{"type": "call", "record": <CallRecord dict>}` — emitted
            after every host function call (prompt, tool, http, …)
          * `{"type": "done", "id": ..., "status": ..., "output": ...}`
            — emitted once when the run finishes

        Mirrors `AgentClient.stream()` in the TypeScript SDK. Uses
        `urllib.request` with line-buffered `readline()` calls, so no
        third-party dependencies are required.

        Usage:

            for evt in client.stream({"question": "hi"}):
                if evt["type"] == "call":
                    print(evt["record"]["function"])
                elif evt["type"] == "done":
                    print(evt["status"], evt["output"])
        """
        url = self.base_url + "/sessions/stream"
        body = json.dumps({"input": input}).encode()
        req = urllib.request.Request(
            url,
            data=body,
            headers={
                "Content-Type": "application/json",
                "Accept": "text/event-stream",
            },
            method="POST",
        )
        try:
            resp = urllib.request.urlopen(req)
        except urllib.error.HTTPError as e:
            err_body = e.read().decode() if e.fp else ""
            raise RuntimeError(f"HTTP {e.code}: {err_body}") from e

        # Minimal SSE parser: accumulate `event:` and `data:` lines until a
        # blank line, then yield the decoded frame. Good enough for the
        # server's tightly-scoped output — we control both ends.
        event_name = "message"
        data_lines: list[str] = []
        try:
            for raw in resp:
                line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                if line == "":
                    if data_lines:
                        payload = "\n".join(data_lines)
                        try:
                            decoded = json.loads(payload)
                        except json.JSONDecodeError:
                            decoded = None
                        if decoded is not None:
                            if event_name == "call":
                                yield {"type": "call", "record": decoded}
                            elif event_name == "done":
                                yield {"type": "done", **decoded}
                    event_name = "message"
                    data_lines = []
                    continue
                if line.startswith(":"):
                    # SSE comment / keep-alive; ignore.
                    continue
                if line.startswith("event:"):
                    event_name = line[len("event:"):].strip()
                elif line.startswith("data:"):
                    data_lines.append(line[len("data:"):].lstrip())
        finally:
            resp.close()

    # -- HTTP helpers --

    def _get(self, path: str) -> dict:
        url = self.base_url + path
        req = urllib.request.Request(url)
        try:
            with urllib.request.urlopen(req) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            body = e.read().decode()
            raise RuntimeError(f"HTTP {e.code}: {body}") from e

    def _post(self, path: str, body: dict) -> dict:
        url = self.base_url + path
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            url, data=data, headers={"Content-Type": "application/json"}
        )
        try:
            with urllib.request.urlopen(req) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            body = e.read().decode()
            raise RuntimeError(f"HTTP {e.code}: {body}") from e
