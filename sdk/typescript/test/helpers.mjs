// Shared test helpers: a tiny stdlib-only mock HTTP server and JSON factories.
//
// The TypeScript SDK is a thin HTTP client, so the tests drive it against a
// `node:http` server we control rather than a real `chidori serve` instance.
// That keeps the suite dependency-free (Node's built-in test runner + http)
// and fast, while still exercising the real request shapes, response parsing,
// SSE streaming, and error handling. End-to-end coverage against the actual
// binary lives in the Python SDK integration tests.

import { createServer } from "node:http";

/**
 * A configurable mock HTTP server. Each test installs a responder via
 * `server.on(...)` and inspects `server.requests` afterwards.
 */
export class MockServer {
  constructor() {
    this._server = null;
    this.baseUrl = "";
    this.requests = [];
    this._responder = (_req, res) => res.writeHead(404).end();
  }

  async start() {
    this._server = createServer((req, res) => {
      const chunks = [];
      req.on("data", (c) => chunks.push(c));
      req.on("end", () => {
        const raw = Buffer.concat(chunks).toString("utf8");
        const url = new URL(req.url ?? "/", "http://localhost");
        this.requests.push({
          method: req.method ?? "GET",
          path: url.pathname,
          body: raw ? safeJson(raw) : null,
        });
        Promise.resolve(this._responder(req, res, { raw, url })).catch(() => {
          if (!res.writableEnded) res.writeHead(500).end();
        });
      });
    });
    await new Promise((resolve) => this._server.listen(0, "127.0.0.1", resolve));
    const { port } = this._server.address();
    this.baseUrl = `http://127.0.0.1:${port}`;
  }

  /** Install the responder for the next request(s). */
  on(responder) {
    this._responder = responder;
  }

  /** Clear recorded requests and reset to a 404 responder. */
  reset() {
    this.requests = [];
    this._responder = (_req, res) => res.writeHead(404).end();
  }

  /** Requests recorded for a given pathname. */
  requestsFor(path) {
    return this.requests.filter((r) => r.path === path);
  }

  async stop() {
    if (!this._server) return;
    await new Promise((resolve, reject) =>
      this._server.close((err) => (err ? reject(err) : resolve())),
    );
    this._server = null;
  }
}

/** Write a JSON response with a status code. */
export function sendJson(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json" });
  res.end(payload);
}

/** Parse JSON, falling back to the raw string when it isn't JSON. */
export function safeJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** A minimal valid CallRecord. */
export function callRecord(overrides = {}) {
  return {
    seq: 1,
    function: "prompt",
    args: { text: "hi" },
    result: "world",
    duration_ms: 12,
    timestamp: "2026-06-21T00:00:00Z",
    ...overrides,
  };
}

/** A minimal valid SnapshotManifest. */
export function snapshotManifest(overrides = {}) {
  return {
    run_id: "run-1",
    abi: { typescript_runtime: 1, quickjs_snapshot: 1, engine_fork: "chidori-js" },
    policy: {
      typescript_imports: "node",
      date: "fixed",
      random: "seeded",
      maps_sets: "reject",
      deterministic_seed: "0",
    },
    entry: { path: "agent.ts", hash: "abc" },
    modules: [],
    call_log_len: 1,
    snapshot_file: "runtime.snapshot",
    created_at: "2026-06-21T00:00:00Z",
    ...overrides,
  };
}
