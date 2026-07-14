// Tests for the AgentClient HTTP surface, Session/Checkpoint helpers, the
// signal outcome distinction, SSE stream parsing, and error handling.
//
// Run with `npm test` (which builds first) or, after `npm run build`,
// `node --test test/*.test.mjs`.

import assert from "node:assert/strict";
import { after, before, beforeEach, describe, it } from "node:test";

import {
  AgentClient,
  AgentClientError,
  Checkpoint,
  ConnectionError,
  HttpError,
  Session,
  TimeoutError,
  isSignalQueued,
} from "../dist/index.js";

import {
  MockServer,
  callRecord,
  sendJson,
  snapshotManifest,
} from "./helpers.mjs";

let server;
let client;

before(async () => {
  server = new MockServer();
  await server.start();
  client = new AgentClient(server.baseUrl);
});

after(async () => {
  await server.stop();
});

beforeEach(() => {
  server.reset();
});

describe("AgentClient construction", () => {
  it("strips a trailing slash from the base URL", () => {
    assert.equal(new AgentClient("http://example.com/").baseUrl, "http://example.com");
    assert.equal(new AgentClient("http://example.com").baseUrl, "http://example.com");
  });

  it("defaults to localhost:8080", () => {
    assert.equal(new AgentClient().baseUrl, "http://localhost:8080");
  });
});

describe("health", () => {
  it("GETs /health and returns the body", async () => {
    server.on((req, res) => {
      assert.equal(req.method, "GET");
      sendJson(res, 200, { status: "ok" });
    });
    assert.deepEqual(await client.health(), { status: "ok" });
    assert.deepEqual(server.requestsFor("/health").length, 1);
  });
});

describe("run", () => {
  it("POSTs the input to /sessions and returns a completed Session", async () => {
    server.on((req, res, { url }) => {
      assert.equal(req.method, "POST");
      assert.equal(url.pathname, "/sessions");
      sendJson(res, 201, {
        id: "run-1",
        status: "completed",
        output: { answer: "forty-two" },
        call_log: [callRecord()],
      });
    });

    const session = await client.run({ question: "what is the answer" });
    assert.ok(session instanceof Session);
    assert.equal(session.id, "run-1");
    assert.equal(session.status, "completed");
    assert.equal(session.ok, true);
    assert.deepEqual(session.output, { answer: "forty-two" });
    assert.deepEqual(session.input, { question: "what is the answer" });

    const sent = server.requestsFor("/sessions")[0].body;
    assert.deepEqual(sent, { input: { question: "what is the answer" } });
  });

  it("sends policy_profile only when a profile is supplied", async () => {
    server.on((_req, res) => sendJson(res, 201, { id: "r", status: "completed" }));

    await client.run({ q: 1 }, { policyProfile: "untrusted" });
    assert.deepEqual(server.requestsFor("/sessions")[0].body, {
      input: { q: 1 },
      policy_profile: "untrusted",
    });

    server.reset();
    server.on((_req, res) => sendJson(res, 201, { id: "r", status: "completed" }));
    await client.run({ q: 2 });
    assert.deepEqual(server.requestsFor("/sessions")[0].body, { input: { q: 2 } });
  });

  it("surfaces a failed run as status=failed with an error", async () => {
    server.on((_req, res) =>
      sendJson(res, 201, { id: "r", status: "failed", error: "boom" }),
    );
    const session = await client.run({ q: 1 });
    assert.equal(session.status, "failed");
    assert.equal(session.ok, false);
    assert.equal(session.error, "boom");
  });
});

describe("replay", () => {
  it("POSTs the call log as replay_from", async () => {
    server.on((_req, res) => sendJson(res, 201, { id: "r", status: "completed", output: 1 }));
    const cp = new Checkpoint("r", { q: 1 }, [callRecord()], null);
    const replayed = await client.replay(cp);
    assert.equal(replayed.status, "completed");
    assert.deepEqual(server.requestsFor("/sessions")[0].body, {
      input: { q: 1 },
      replay_from: [callRecord()],
    });
  });
});

describe("resume", () => {
  it("POSTs the response to /sessions/{id}/resume", async () => {
    server.on((req, res, { url }) => {
      assert.equal(url.pathname, "/sessions/run-9/resume");
      sendJson(res, 200, {
        id: "run-9",
        status: "completed",
        input: { action: "delete" },
        output: { approved: true },
      });
    });
    const resumed = await client.resume("run-9", "yes");
    assert.equal(resumed.status, "completed");
    assert.deepEqual(resumed.output, { approved: true });
    assert.deepEqual(server.requestsFor("/sessions/run-9/resume")[0].body, {
      response: "yes",
    });
  });
});

describe("signal", () => {
  it("returns a resumed Session when the run was waiting on the name (200)", async () => {
    server.on((req, res, { url }) => {
      assert.equal(url.pathname, "/sessions/run-3/signal");
      sendJson(res, 200, {
        id: "run-3",
        status: "completed",
        output: { decision: "approve" },
      });
    });

    const result = await client.signal("run-3", {
      name: "review",
      payload: { decision: "approve" },
      from: { kind: "human", id: "mara" },
    });

    assert.equal(isSignalQueued(result), false);
    assert.ok(result instanceof Session);
    assert.equal(result.status, "completed");
    assert.deepEqual(result.output, { decision: "approve" });

    // payload/from default through even when partially specified.
    assert.deepEqual(server.requestsFor("/sessions/run-3/signal")[0].body, {
      name: "review",
      payload: { decision: "approve" },
      from: { kind: "human", id: "mara" },
    });
  });

  it("returns a SignalQueued descriptor when the signal is enqueued (202)", async () => {
    server.on((_req, res) =>
      sendJson(res, 202, {
        id: "run-3",
        status: "queued",
        name: "steer",
        delivery_seq: 7,
      }),
    );

    const result = await client.signal("run-3", { name: "steer" });
    assert.equal(isSignalQueued(result), true);
    assert.equal(result instanceof Session, false);
    assert.equal(result.status, "queued");
    assert.equal(result.name, "steer");
    assert.equal(result.delivery_seq, 7);

    // Unspecified payload/from default to null in the request body.
    assert.deepEqual(server.requestsFor("/sessions/run-3/signal")[0].body, {
      name: "steer",
      payload: null,
      from: null,
    });
  });

  it("treats a delivered_live 200 body as queued", async () => {
    server.on((_req, res) =>
      sendJson(res, 200, {
        id: "run-3",
        status: "delivered_live",
        name: "steer",
        delivery_seq: 9,
      }),
    );
    const result = await client.signal("run-3", { name: "steer" });
    assert.equal(isSignalQueued(result), true);
    assert.equal(result.status, "delivered_live");
  });

  it("throws on a terminal run (409)", async () => {
    server.on((_req, res) => sendJson(res, 409, { error: "run is terminal" }));
    await assert.rejects(() => client.signal("run-3", { name: "review" }), /HTTP 409/);
  });
});

describe("session reads", () => {
  it("getSession fetches /sessions/{id}", async () => {
    server.on((_req, res) =>
      sendJson(res, 200, { id: "s1", status: "paused", input: { a: 1 }, pending_prompt: "ok?" }),
    );
    const session = await client.getSession("s1");
    assert.equal(session.status, "paused");
    assert.equal(session.pendingPrompt, "ok?");
    assert.deepEqual(session.input, { a: 1 });
  });

  it("listSessions returns the summaries array", async () => {
    server.on((_req, res) =>
      sendJson(res, 200, { sessions: [{ id: "a", status: "completed" }, { id: "b", status: "failed", error: "x" }] }),
    );
    const sessions = await client.listSessions();
    assert.equal(sessions.length, 2);
    assert.equal(sessions[0].id, "a");
    assert.equal(sessions[1].error, "x");
  });

  it("getCheckpoint returns call log + manifest", async () => {
    server.on((_req, res) =>
      sendJson(res, 200, { call_log: [callRecord()], snapshot_manifest: snapshotManifest() }),
    );
    const data = await client.getCheckpoint("s1");
    assert.equal(data.call_log.length, 1);
    assert.equal(data.snapshot_manifest.run_id, "run-1");
  });

  it("getSnapshotManifest unwraps the manifest", async () => {
    server.on((_req, res) =>
      sendJson(res, 200, { snapshot_manifest: snapshotManifest({ run_id: "abc" }) }),
    );
    const manifest = await client.getSnapshotManifest("s1");
    assert.equal(manifest.run_id, "abc");
    assert.equal(manifest.snapshot_file, "runtime.snapshot");
  });
});

describe("Session helpers", () => {
  it("checkpoint() lazily fetches the call log when empty", async () => {
    server.on((req, res, { url }) => {
      if (req.method === "POST" && url.pathname === "/sessions") {
        return sendJson(res, 201, { id: "run-7", status: "completed", output: { ok: true } });
      }
      if (req.method === "GET" && url.pathname === "/sessions/run-7/checkpoint") {
        return sendJson(res, 200, { call_log: [callRecord()], snapshot_manifest: snapshotManifest() });
      }
      res.writeHead(404).end();
    });

    const session = await client.run({ q: 1 });
    assert.equal(session.callLog.length, 0);

    const cp = await session.checkpoint();
    assert.ok(cp instanceof Checkpoint);
    assert.equal(cp.sessionId, "run-7");
    assert.equal(cp.callLog.length, 1);
    assert.equal(cp.snapshotManifest.run_id, "run-1");
    assert.equal(server.requestsFor("/sessions/run-7/checkpoint").length, 1);
  });

  it("replay() round-trips a session through the client", async () => {
    server.on((req, res, { url }) => {
      if (req.method === "POST" && url.pathname === "/sessions" && server.requests.length === 1) {
        // initial run
        return sendJson(res, 201, {
          id: "run-8",
          status: "completed",
          output: { v: 1 },
          call_log: [callRecord()],
          snapshot_manifest: snapshotManifest(),
        });
      }
      if (req.method === "POST" && url.pathname === "/sessions") {
        // the replay
        return sendJson(res, 201, { id: "run-8", status: "completed", output: { v: 1 } });
      }
      res.writeHead(404).end();
    });

    const session = await client.run({ q: 1 });
    const replayed = await session.replay();
    assert.equal(replayed.status, "completed");
    const replayReq = server.requestsFor("/sessions")[1].body;
    assert.deepEqual(replayReq.replay_from, [callRecord()]);
  });

  it("replay() without a bound client throws", async () => {
    const orphan = new Session("x", "completed", { q: 1 });
    await assert.rejects(() => orphan.replay(), /no client bound/);
  });
});

describe("Checkpoint serialization", () => {
  it("round-trips through toJSON / fromJSON", () => {
    const cp = new Checkpoint("s1", { q: 1 }, [callRecord()], snapshotManifest());
    const round = Checkpoint.fromJSON(cp.toJSON());
    assert.deepEqual(round.toJSON(), cp.toJSON());
    assert.equal(round.sessionId, "s1");
    assert.equal(round.snapshotManifest.run_id, "run-1");
  });

  it("omits snapshot_manifest when absent", () => {
    const cp = new Checkpoint("s1", { q: 1 }, [callRecord()], null);
    assert.equal("snapshot_manifest" in cp.toJSON(), false);
    const round = Checkpoint.fromJSON(cp.toJSON());
    assert.equal(round.snapshotManifest, null);
  });
});

describe("error handling", () => {
  it("throws a typed HttpError carrying status, body, and detail", async () => {
    // 400 is not retryable, so a single handler suffices.
    server.on((_req, res) => sendJson(res, 400, { error: "kaboom" }));
    await assert.rejects(() => client.health(), (err) => {
      assert.ok(err instanceof HttpError);
      assert.ok(err instanceof AgentClientError);
      assert.equal(err.status, 400);
      assert.equal(err.detail, "kaboom");
      assert.match(err.body, /kaboom/);
      assert.match(err.message, /HTTP 400/);
      assert.match(err.message, /kaboom/);
      return true;
    });
  });

  it("distinguishes 400/404/409 via err.status", async () => {
    for (const status of [400, 404, 409]) {
      server.reset();
      server.on((_req, res) => sendJson(res, status, { error: `status ${status}` }));
      await assert.rejects(
        () => client.signal("run-x", { name: "review" }),
        (err) => err instanceof HttpError && err.status === status,
      );
    }
  });

  it("does not retry POSTs on a retryable status", async () => {
    server.on((_req, res) => sendJson(res, 503, { error: "overloaded" }));
    await assert.rejects(() => client.run({ q: 1 }), (err) => {
      assert.ok(err instanceof HttpError);
      assert.equal(err.status, 503);
      return true;
    });
    assert.equal(server.requestsFor("/sessions").length, 1);
  });

  it("retries GETs on a retryable status and succeeds", async () => {
    const fastClient = new AgentClient(server.baseUrl, { retries: 2, retryDelayMs: 1 });
    let calls = 0;
    server.on((_req, res) => {
      calls += 1;
      if (calls < 3) return sendJson(res, 503, { error: "warming up" });
      sendJson(res, 200, { status: "ok" });
    });
    assert.deepEqual(await fastClient.health(), { status: "ok" });
    assert.equal(calls, 3);
  });

  it("gives up retrying GETs after `retries` attempts", async () => {
    const fastClient = new AgentClient(server.baseUrl, { retries: 1, retryDelayMs: 1 });
    server.on((_req, res) => sendJson(res, 503, { error: "still down" }));
    await assert.rejects(() => fastClient.health(), (err) => {
      assert.ok(err instanceof HttpError);
      assert.equal(err.status, 503);
      return true;
    });
    assert.equal(server.requestsFor("/health").length, 2);
  });

  it("does not retry GETs on a non-retryable status", async () => {
    server.on((_req, res) => sendJson(res, 404, { error: "no such session" }));
    await assert.rejects(() => client.getSession("nope"), (err) => {
      assert.ok(err instanceof HttpError);
      assert.equal(err.status, 404);
      return true;
    });
    assert.equal(server.requestsFor("/sessions/nope").length, 1);
  });

  it("throws a TimeoutError when the server never responds", async () => {
    const impatient = new AgentClient(server.baseUrl, { timeoutMs: 50, retries: 0 });
    server.on(() => {
      // Never respond; the client should abort.
    });
    await assert.rejects(() => impatient.run({ q: 1 }), (err) => {
      assert.ok(err instanceof TimeoutError);
      assert.ok(err instanceof AgentClientError);
      assert.equal(err.timeoutMs, 50);
      return true;
    });
  });

  it("throws a ConnectionError when nothing is listening", async () => {
    const nowhere = new AgentClient("http://127.0.0.1:1", { retries: 0, timeoutMs: 1000 });
    await assert.rejects(() => nowhere.health(), (err) => {
      assert.ok(err instanceof ConnectionError);
      assert.ok(err instanceof AgentClientError);
      return true;
    });
  });
});

describe("stream", () => {
  it("parses call, prompt lifecycle, and done SSE events", async () => {
    server.on((req, res, { url }) => {
      assert.equal(url.pathname, "/sessions/stream");
      res.writeHead(200, { "content-type": "text/event-stream" });
      const frame = (event, data) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
      res.write(frame("call", callRecord({ seq: 1, function: "prompt" })));
      res.write(frame("prompt_start", { stream_id: "s1", seq: 1, model: "m" }));
      res.write(frame("prompt_delta", { stream_id: "s1", seq: 1, delta: "forty-" }));
      res.write(frame("prompt_delta", { stream_id: "s1", seq: 1, delta: "two" }));
      res.write(frame("prompt_end", { stream_id: "s1", seq: 1 }));
      res.write(frame("done", { id: "run-1", status: "completed", output: { answer: "forty-two" } }));
      res.end();
    });

    const events = [];
    for await (const evt of client.stream({ question: "stream me" })) {
      events.push(evt);
    }

    const calls = events.filter((e) => e.type === "call");
    const deltas = events.filter((e) => e.type === "prompt_delta");
    const done = events.filter((e) => e.type === "done");
    assert.equal(calls.length, 1);
    assert.equal(calls[0].record.function, "prompt");
    assert.equal(calls[0].record.seq, 1);
    assert.equal(deltas.length, 2);
    assert.deepEqual(deltas.map((d) => d.delta), ["forty-", "two"]);
    assert.equal(done.length, 1);
    assert.equal(done[0].status, "completed");
    assert.deepEqual(done[0].output, { answer: "forty-two" });

    assert.deepEqual(server.requestsFor("/sessions/stream")[0].body, {
      input: { question: "stream me" },
    });
  });

  it("reassembles an SSE frame split across chunks", async () => {
    server.on((_req, res) => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      // Deliberately split one done frame across two writes.
      res.write('event: done\ndata: {"id":"run-1",');
      res.write('"status":"completed","output":{"v":1}}\n\n');
      res.end();
    });

    const events = [];
    for await (const evt of client.stream({})) events.push(evt);
    assert.equal(events.length, 1);
    assert.equal(events[0].type, "done");
    assert.deepEqual(events[0].output, { v: 1 });
  });

  it("throws an HttpError when the stream request fails", async () => {
    server.on((_req, res) => res.writeHead(500).end());
    await assert.rejects(async () => {
      // eslint-disable-next-line no-unused-vars
      for await (const _ of client.stream({})) {
        // no-op
      }
    }, (err) => {
      assert.ok(err instanceof HttpError);
      assert.equal(err.status, 500);
      return true;
    });
  });
});
