/**
 * chidori TypeScript SDK — HTTP client for a running `chidori serve`
 * instance. Mirrors the Python SDK (`sdk/python/chidori`). Zero runtime
 * dependencies; uses the global `fetch` available in Node 18+ and browsers.
 */

import type { SignalSender } from "./agent.js";

export type {
  ActorHandle,
  ActorMessage,
  ActorOutcome,
  ActorOutcomeStatus,
  ActorRestartStrategy,
  Actors,
  ActorStatus,
  ActorStillRunning,
  AgentFunction,
  AgentJson,
  AgentOutput,
  AppData,
  BranchOptions,
  BranchOutcome,
  BranchStatus,
  BranchVariant,
  CacheTtl,
  Chidori,
  ChidoriUtil,
  CompactOptions,
  Context,
  Conversation,
  ConversationLoopOptions,
  ConversationOptions,
  ConversationTurn,
  DatePolicy,
  DetachedAgentHandle,
  DetachedAgentOutcome,
  DetachedAgents,
  DetachedAgentStatus,
  InputOptions,
  JoinActorOptions,
  JsonObject,
  JsonSchema,
  LlmResponseJson,
  LogFields,
  MapSetSnapshotPolicy,
  MemoryStore,
  ParallelOptions,
  PromptOptions,
  PromptStreamType,
  RandomPolicy,
  ReceiveOptions,
  RetryOptions,
  RuntimePolicyConfig,
  Signal,
  SignalOptions,
  SignalSender,
  SignalTimeout,
  SpawnActorOptions,
  SpawnAgentOptions,
  ToolDefinition,
  ToolFunction,
  TryCallResult,
  TypeScriptImportPolicy,
  WorkspaceEntry,
  WorkspaceFileStatus,
  WorkspaceHost,
  WorkspaceListOptions,
  WorkspaceWriteOptions,
} from "./agent.js";

// Authoring entrypoints: the host object and the `run(handler)` definer. These
// are value exports (the runtime strips the import and supplies them).
export { chidori, run } from "./agent.js";

/** JSON-serialisable value — what agents produce as output and accept as input. */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** Server-side session status. */
export type SessionStatus =
  | "running"
  | "completed"
  | "failed"
  | "paused"
  | "cancelled"
  | "awaitingapproval";

/**
 * Built-in policy profiles selectable per session. Layered on the server
 * policy with stricter-wins semantics: a profile can tighten what the
 * operator's policy allows, never relax it.
 */
export type PolicyProfile = "untrusted" | "supervised";

/** A single host function call recorded during an agent run. */
export interface CallRecord {
  seq: number;
  function: string;
  args: Json;
  result: Json;
  duration_ms: number;
  timestamp: string;
  token_usage?: { input_tokens: number; output_tokens: number };
  error?: string;
}

/** Source hash recorded in a runtime snapshot manifest. */
export interface SnapshotSourceFingerprint {
  path: string;
  hash: string;
}

/** Snapshot ABI recorded before loading VM snapshot bytes. */
export interface SnapshotAbi {
  typescript_runtime: number;
  quickjs_snapshot: number;
  engine_fork: string;
}

/** Determinism policy captured with a runtime snapshot. */
export interface SnapshotRuntimePolicy {
  typescript_imports: "none" | "relative" | "project";
  date: "disabled" | "fixed" | "host";
  random: "disabled" | "seeded" | "host";
  maps_sets: "reject" | "serialize";
  deterministic_seed: string;
}

/** Pending host operation captured at a durable snapshot safepoint. */
export interface PendingHostOperation {
  id: number;
  seq: number;
  kind:
    | "prompt"
    | "input"
    | "policy_approval"
    | "tool"
    | "call_agent"
    | "http"
    | "template"
    | "memory"
    | "checkpoint";
  args: Json;
  created_at: string;
}

export type HostPromiseState =
  | "pending"
  | { resolved: { value: Json; completed_at: string } }
  | { rejected: { error: string; completed_at: string } };

export interface HostPromiseRecord {
  operation: PendingHostOperation;
  state: HostPromiseState;
}

/**
 * Public snapshot metadata. It is safe to expose in SDK checkpoints; the raw
 * `runtime.snapshot` VM bytes stay server-side unless an admin endpoint opts in.
 */
export interface SnapshotManifest {
  run_id: string;
  abi: SnapshotAbi;
  policy: SnapshotRuntimePolicy;
  entry: SnapshotSourceFingerprint;
  modules: SnapshotSourceFingerprint[];
  pending?: PendingHostOperation | null;
  host_promises?: HostPromiseRecord[];
  call_log_len: number;
  snapshot_file: string;
  created_at: string;
}

/**
 * A saved checkpoint — session id, input, the full call log, and optional
 * runtime snapshot metadata. The call log is enough for deterministic replay;
 * the snapshot manifest lets clients inspect durable-resume state without
 * downloading raw VM snapshot bytes.
 */
export class Checkpoint {
  constructor(
    public readonly sessionId: string,
    public readonly input: Json,
    public readonly callLog: CallRecord[],
    public readonly snapshotManifest: SnapshotManifest | null = null,
  ) {}

  /** Serialise to JSON. Pairs with `Checkpoint.fromJSON`. */
  toJSON(): {
    session_id: string;
    input: Json;
    call_log: CallRecord[];
    snapshot_manifest?: SnapshotManifest | null;
  } {
    return {
      session_id: this.sessionId,
      input: this.input,
      call_log: this.callLog,
      ...(this.snapshotManifest ? { snapshot_manifest: this.snapshotManifest } : {}),
    };
  }

  static fromJSON(data: {
    session_id: string;
    input: Json;
    call_log: CallRecord[];
    snapshot_manifest?: SnapshotManifest | null;
  }): Checkpoint {
    return new Checkpoint(
      data.session_id,
      data.input,
      data.call_log,
      data.snapshot_manifest ?? null,
    );
  }
}

/**
 * Returned by `client.signal` when the signal was accepted but did not resolve
 * a pause synchronously. Mirrors the server's 202 Accepted body:
 *   * `"queued"` — the run was not waiting on this name; the signal sits in
 *     the durable mailbox until a matching listen point drains it.
 *   * `"delivered_live"` — a live streaming worker supervises the run; the
 *     signal was enqueued into the running agent's in-memory mailbox and the
 *     worker was woken to resume a matching pause in-process.
 */
export interface SignalQueued {
  /** Session id the signal was delivered to. */
  id: string;
  status: "queued" | "delivered_live";
  /** The signal name, echoed back. */
  name: string;
  /** Monotonic per-run sequence freezing global arrival order across senders. */
  delivery_seq: number;
}

/** A signal delivery request body for `client.signal`. */
export interface SignalDelivery {
  /** Required, non-empty: the listen-point name the agent awaits. */
  name: string;
  /** Any JSON payload (default null). */
  payload?: Json;
  /** Sender provenance recorded in the trace (default null). */
  from?: SignalSender | Json;
}

/**
 * Type guard distinguishing the two `client.signal` outcomes: a resumed
 * `Session` (the run was paused-waiting on this name) vs a `SignalQueued`
 * descriptor (the signal was enqueued for a later listen point).
 */
export function isSignalQueued(result: Session | SignalQueued): result is SignalQueued {
  const status = (result as SignalQueued).status;
  return status === "queued" || status === "delivered_live";
}

/** One execution of an agent — result + call log + status. */
export class Session {
  constructor(
    public readonly id: string,
    public status: SessionStatus,
    public readonly input: Json,
    public output: Json | null = null,
    public error: string | null = null,
    public callLog: CallRecord[] = [],
    public pendingPrompt: string | null = null,
    private readonly client: AgentClient | null = null,
    public snapshotManifest: SnapshotManifest | null = null,
    /**
     * When the run is `paused` at a `chidori.signal(name)` listen point, the
     * name it is waiting on (so a caller can deliver via `client.signal`).
     * `null` for plain `input()` pauses and non-signal states.
     */
    public pendingSignalName: string | null = null,
    /**
     * The full awaited name set when paused on a signal listen point: `[name]`
     * for `chidori.signal(name)`, the listen set for the fan-in `chidori.signal(names)`.
     * Empty for non-signal states.
     */
    public pendingSignalNames: string[] = [],
    /**
     * Absolute deadline (ISO timestamp) for a signal pause created with
     * `timeoutMs`; the server resolves the pause with the timeout sentinel
     * when it passes. `null` when the pause has no timeout.
     */
    public pendingSignalDeadline: string | null = null,
    /**
     * The artifact under review for an `input()` pause created with
     * `{ details }` (a draft, a diff) — surface it so a human never approves
     * blind. `null` when the pause carries no details.
     */
    public pendingDetails: string | null = null,
    /**
     * The durable run directory id (`.chidori/runs/<run_id>`) this session
     * journals into. Deliberately distinct from the session id: `chidori
     * resume <agent.ts> <run_id>` and `chidori trace <run_id>` take THIS id.
     */
    public runId: string | null = null,
  ) {}

  get ok(): boolean {
    return this.status === "completed";
  }

  /**
   * Fetch the full call log from the server (if not already loaded) and
   * wrap it in a Checkpoint suitable for saving / later replay.
   */
  async checkpoint(): Promise<Checkpoint> {
    if ((this.callLog.length === 0 || this.snapshotManifest === null) && this.client) {
      const data = await this.client.getCheckpoint(this.id);
      this.callLog = data.call_log;
      this.snapshotManifest = data.snapshot_manifest ?? null;
    }
    return new Checkpoint(this.id, this.input, this.callLog, this.snapshotManifest);
  }

  /** Replay this session through the server; same inputs, cached results. */
  async replay(): Promise<Session> {
    if (!this.client) {
      throw new Error("Session has no client bound; use client.replay()");
    }
    const cp = await this.checkpoint();
    return this.client.replay(cp);
  }
}

/** Stream event yielded by `AgentClient.stream`. */
export type StreamEvent =
  | { type: "call"; record: CallRecord }
  | {
      type: "prompt_start";
      stream_id: string;
      seq: number;
      prompt_type?: string | null;
      model: string;
    }
  | {
      type: "prompt_delta";
      stream_id: string;
      seq: number;
      prompt_type?: string | null;
      delta: string;
    }
  | {
      type: "prompt_end";
      stream_id: string;
      seq: number;
      prompt_type?: string | null;
      error?: string | null;
    }
  | {
      /**
       * The streamed run paused at a `signal()` listen point
       * and stays live: the worker keeps supervising, and a delivered signal
       * (or the `timeoutMs` deadline) resumes it in-process — further events
       * follow on the same stream. Deliver with `client.signal`.
       */
      type: "paused";
      id: string;
      status: "paused";
      pending_seq: number;
      pending_signal_name?: string | null;
      pending_signal_names?: string[];
      pending_signal_deadline?: string | null;
    }
  | { type: "done"; id: string; status: SessionStatus; output?: Json; error?: string };

/** Base class for every error the SDK throws. `catch (e) { if (e instanceof
 * AgentClientError) ... }` covers HTTP failures, timeouts, and connection
 * errors alike. */
export class AgentClientError extends Error {}

/**
 * A non-2xx HTTP response. Carries the parsed `status` so callers can
 * distinguish the server's documented semantics — e.g. for `client.signal`:
 * 400 (empty name), 404 (unknown session), 409 (terminal run) — instead of
 * string-matching the message.
 */
export class HttpError extends AgentClientError {
  constructor(
    /** HTTP method of the failed request. */
    readonly method: string,
    /** Request path relative to the base URL. */
    readonly path: string,
    /** HTTP status code (400, 404, 409, 500, ...). */
    readonly status: number,
    /** Raw response body text (may be empty). */
    readonly body: string,
    /** The server's `error` field, when the body was `{ "error": ... }`. */
    readonly detail: string | null = null,
  ) {
    super(`${method} ${path} failed: HTTP ${status}${detail || body ? `: ${detail ?? body}` : ""}`);
    this.name = "HttpError";
  }

  static async fromResponse(method: string, path: string, resp: Response): Promise<HttpError> {
    const body = await resp.text().catch(() => "");
    let detail: string | null = null;
    try {
      const parsed = JSON.parse(body) as { error?: unknown };
      if (typeof parsed.error === "string") detail = parsed.error;
    } catch {
      // not JSON — leave detail null
    }
    return new HttpError(method, path, resp.status, body, detail);
  }
}

/** The request exceeded the client's `timeoutMs` without completing. */
export class TimeoutError extends AgentClientError {
  constructor(
    readonly method: string,
    readonly path: string,
    readonly timeoutMs: number,
  ) {
    super(`${method} ${path} timed out after ${timeoutMs}ms`);
    this.name = "TimeoutError";
  }
}

/** The request never produced an HTTP response (refused, reset, DNS, ...). */
export class ConnectionError extends AgentClientError {
  constructor(
    readonly method: string,
    readonly path: string,
    cause: unknown,
  ) {
    super(`${method} ${path} failed: ${cause instanceof Error ? cause.message : String(cause)}`, {
      cause,
    });
    this.name = "ConnectionError";
  }
}

/** Response statuses worth retrying on idempotent requests. */
const RETRYABLE_STATUS = new Set([429, 502, 503, 504]);

export interface AgentClientOptions {
  /**
   * Per-request timeout in milliseconds; `0` disables it. Defaults to
   * 300 000 (5 minutes) — generous because `run()` executes the whole agent
   * before responding, but finite so a hung server surfaces as a
   * `TimeoutError` instead of blocking forever. For `stream()` the timeout
   * covers connection establishment only, never an open event stream.
   */
  timeoutMs?: number;
  /**
   * How many times to retry **idempotent GET requests** after a connection
   * error, timeout, or retryable status (429/502/503/504). Defaults to 2.
   * POST requests are never retried — `run`/`resume`/`signal` are not
   * idempotent, and a blind retry could execute an agent twice.
   */
  retries?: number;
  /** Base delay between retries in milliseconds, doubling per attempt (default 250). */
  retryDelayMs?: number;
}

/**
 * HTTP client for an `chidori serve` instance.
 *
 * ```ts
 * const client = new AgentClient("http://localhost:8080");
 * const session = await client.run({ document: "Rust is a systems language." });
 * console.log(session.output);
 *
 * const cp = await session.checkpoint();
 * const replayed = await client.replay(cp);  // zero LLM calls
 * ```
 *
 * Failures throw typed errors (all extending {@link AgentClientError}):
 * {@link HttpError} with a `.status` for non-2xx responses,
 * {@link TimeoutError} after `timeoutMs`, {@link ConnectionError} when no
 * response arrived at all.
 */
export class AgentClient {
  readonly baseUrl: string;
  readonly timeoutMs: number;
  readonly retries: number;
  readonly retryDelayMs: number;

  constructor(baseUrl: string = "http://localhost:8080", options: AgentClientOptions = {}) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.timeoutMs = options.timeoutMs ?? 300_000;
    this.retries = options.retries ?? 2;
    this.retryDelayMs = options.retryDelayMs ?? 250;
  }

  async health(): Promise<Json> {
    return (await this.getJSON("/health")) as Json;
  }

  /**
   * Create a new session and run the agent with the given input.
   *
   * `options.policyProfile` optionally names a built-in policy profile
   * ("untrusted" or "supervised") applied to every run of this session.
   * It is layered on the server policy with stricter-wins semantics — it
   * can tighten what the operator allows, never relax it. Under
   * "supervised", gated calls pause the session as "awaitingapproval";
   * approve or deny them via the server's /approve endpoint.
   */
  async run(input: Json, options?: { policyProfile?: PolicyProfile }): Promise<Session> {
    const body: Record<string, unknown> = { input };
    if (options?.policyProfile) {
      body.policy_profile = options.policyProfile;
    }
    const data = await this.postJSON("/sessions", body);
    return this.sessionFrom(data, input);
  }

  /** Replay an agent from a saved checkpoint. */
  async replay(checkpoint: Checkpoint): Promise<Session> {
    const data = await this.postJSON("/sessions", {
      input: checkpoint.input,
      replay_from: checkpoint.callLog,
    });
    return this.sessionFrom(data, checkpoint.input);
  }

  /**
   * Supply a response to a paused `input()` call and continue the run.
   * The same session id advances to completed (or re-pauses on a later
   * `input()`).
   */
  async resume(sessionId: string, response: string): Promise<Session> {
    const data = await this.postJSON(`/sessions/${sessionId}/resume`, {
      response,
    });
    return this.sessionFrom(data, (data.input as Json | undefined) ?? null);
  }

  /**
   * Deliver a signal `{ name, payload?, from? }` to a run
   * (`POST /sessions/{id}/signal`).
   *
   * Two outcomes, distinguished by `isSignalQueued`:
   *   * the run was paused-waiting on this exact name → it resolves the pause
   *     and resumes; this returns the advanced `Session` (200), now `completed`
   *     or re-`paused`.
   *   * otherwise → the signal is accepted asynchronously; this returns a
   *     `SignalQueued` descriptor (202) carrying the assigned `delivery_seq`,
   *     with `status` `"queued"` (durable mailbox) or `"delivered_live"` (a
   *     live streaming worker received it in-memory and resumes a matching
   *     pause in-process).
   *
   * Throws on 400 (empty name), 404 (unknown session), or 409 (terminal run).
   */
  async signal(
    sessionId: string,
    delivery: SignalDelivery,
  ): Promise<Session | SignalQueued> {
    const { status, data } = await this.postJSONWithStatus(
      `/sessions/${sessionId}/signal`,
      {
        name: delivery.name,
        payload: delivery.payload ?? null,
        from: delivery.from ?? null,
      },
    );
    const accepted = (data as { status?: string }).status;
    if (status === 202 || accepted === "queued" || accepted === "delivered_live") {
      return data as unknown as SignalQueued;
    }
    return this.sessionFrom(data, (data.input as Json | undefined) ?? null);
  }

  /** Fetch an existing session by id. */
  async getSession(id: string): Promise<Session> {
    const data = (await this.getJSON(`/sessions/${id}`)) as Record<string, unknown>;
    return this.sessionFrom(data, (data.input as Json | undefined) ?? null);
  }

  /** List all sessions. Returns the raw summaries. */
  async listSessions(): Promise<Array<{ id: string; status: SessionStatus; error?: string }>> {
    const data = (await this.getJSON("/sessions")) as {
      sessions: Array<{ id: string; status: SessionStatus; error?: string }>;
    };
    return data.sessions;
  }

  /** Fetch the full call log and optional snapshot manifest for a session. */
  async getCheckpoint(id: string): Promise<{
    call_log: CallRecord[];
    snapshot_manifest?: SnapshotManifest | null;
  }> {
    return (await this.getJSON(`/sessions/${id}/checkpoint`)) as {
      call_log: CallRecord[];
      snapshot_manifest?: SnapshotManifest | null;
    };
  }

  /** Fetch only the snapshot manifest metadata for a session, never VM bytes. */
  async getSnapshotManifest(id: string): Promise<SnapshotManifest> {
    const data = (await this.getJSON(`/sessions/${id}/snapshot`)) as {
      snapshot_manifest: SnapshotManifest;
    };
    return data.snapshot_manifest;
  }

  /**
   * Stream an agent run: yields host function calls, prompt stream lifecycle
   * events (`prompt_start`, `prompt_delta`, `prompt_end`), then a final
   * `done` event. Prompt events include `prompt_type` so UIs can filter
   * progress streams separately from final-answer streams.
   */
  async *stream(input: Json): AsyncGenerator<StreamEvent, void, void> {
    // The timeout covers connection establishment (until response headers
    // arrive), not the open event stream — a healthy run may stream for a
    // long time between events.
    const controller = new AbortController();
    const timer =
      this.timeoutMs > 0 ? setTimeout(() => controller.abort(), this.timeoutMs) : null;
    let resp: Response;
    try {
      resp = await fetch(`${this.baseUrl}/sessions/stream`, {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
        body: JSON.stringify({ input }),
        signal: controller.signal,
      });
    } catch (err) {
      throw controller.signal.aborted
        ? new TimeoutError("POST", "/sessions/stream", this.timeoutMs)
        : new ConnectionError("POST", "/sessions/stream", err);
    } finally {
      if (timer) clearTimeout(timer);
    }
    if (!resp.ok || !resp.body) {
      throw await HttpError.fromResponse("POST", "/sessions/stream", resp);
    }

    // Minimal SSE parser — just enough for the events our server emits.
    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let idx: number;
      while ((idx = buffer.indexOf("\n\n")) !== -1) {
        const frame = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        const parsed = parseSseFrame(frame);
        if (parsed) yield parsed;
      }
    }
  }

  // -- internals ----------------------------------------------------------

  private sessionFrom(data: Record<string, unknown>, input: Json): Session {
    return new Session(
      data.id as string,
      (data.status as SessionStatus) ?? "failed",
      input,
      (data.output as Json | undefined) ?? null,
      (data.error as string | undefined) ?? null,
      (data.call_log as CallRecord[] | undefined) ?? [],
      (data.pending_prompt as string | undefined) ?? null,
      this,
      (data.snapshot_manifest as SnapshotManifest | undefined) ?? null,
      (data.pending_signal_name as string | undefined) ?? null,
      (data.pending_signal_names as string[] | undefined) ?? [],
      (data.pending_signal_deadline as string | undefined) ?? null,
      (data.pending_details as string | undefined) ?? null,
      (data.run_id as string | undefined) ?? null,
    );
  }

  private async getJSON(path: string): Promise<unknown> {
    // GETs are idempotent: retry connection failures, timeouts, and
    // retryable statuses with exponential backoff.
    const resp = await this.request("GET", path, undefined, this.retries);
    return await resp.json();
  }

  private async postJSON(path: string, body: unknown): Promise<Record<string, unknown>> {
    return (await this.postJSONWithStatus(path, body)).data;
  }

  private async postJSONWithStatus(
    path: string,
    body: unknown,
  ): Promise<{ status: number; data: Record<string, unknown> }> {
    // POSTs are never retried: run/resume/signal are not idempotent.
    const resp = await this.request("POST", path, body, 0);
    return { status: resp.status, data: (await resp.json()) as Record<string, unknown> };
  }

  /**
   * One HTTP exchange with timeout and (for idempotent requests) retries.
   * Resolves with a 2xx `Response`; throws {@link HttpError},
   * {@link TimeoutError}, or {@link ConnectionError} otherwise.
   */
  private async request(
    method: "GET" | "POST",
    path: string,
    body: unknown,
    retries: number,
  ): Promise<Response> {
    let lastError: AgentClientError | null = null;
    for (let attempt = 0; attempt <= retries; attempt++) {
      if (attempt > 0) {
        await sleep(this.retryDelayMs * 2 ** (attempt - 1));
      }
      const controller = new AbortController();
      const timer =
        this.timeoutMs > 0 ? setTimeout(() => controller.abort(), this.timeoutMs) : null;
      try {
        let resp: Response;
        try {
          resp = await fetch(this.baseUrl + path, {
            method,
            signal: controller.signal,
            ...(body !== undefined
              ? {
                  headers: { "Content-Type": "application/json" },
                  body: JSON.stringify(body),
                }
              : {}),
          });
        } catch (err) {
          throw controller.signal.aborted
            ? new TimeoutError(method, path, this.timeoutMs)
            : new ConnectionError(method, path, err);
        }
        if (!resp.ok) throw await HttpError.fromResponse(method, path, resp);
        return resp;
      } catch (err) {
        const retryable =
          err instanceof TimeoutError ||
          err instanceof ConnectionError ||
          (err instanceof HttpError && RETRYABLE_STATUS.has(err.status));
        if (!retryable || attempt === retries) throw err;
        lastError = err as AgentClientError;
      } finally {
        if (timer) clearTimeout(timer);
      }
    }
    // Unreachable: the loop either returns or throws on the last attempt.
    throw lastError ?? new ConnectionError(method, path, "retries exhausted");
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseSseFrame(frame: string): StreamEvent | null {
  let event = "message";
  const dataLines: string[] = [];
  for (const line of frame.split("\n")) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) dataLines.push(line.slice(5).trim());
  }
  if (dataLines.length === 0) return null;
  try {
    const data = JSON.parse(dataLines.join("\n"));
    if (event === "call") return { type: "call", record: data as CallRecord };
    if (event === "prompt_start") {
      return { type: "prompt_start", ...(data as object) } as StreamEvent;
    }
    if (event === "prompt_delta") {
      return { type: "prompt_delta", ...(data as object) } as StreamEvent;
    }
    if (event === "prompt_end") {
      return { type: "prompt_end", ...(data as object) } as StreamEvent;
    }
    if (event === "paused") {
      return { type: "paused", ...(data as object) } as StreamEvent;
    }
    if (event === "done") return { type: "done", ...(data as object) } as StreamEvent;
  } catch {
    return null;
  }
  return null;
}
