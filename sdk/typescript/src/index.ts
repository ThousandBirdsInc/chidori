/**
 * chidori TypeScript SDK — HTTP client for a running `chidori serve`
 * instance. Mirrors the Python SDK (`sdk/python/chidori`). Zero runtime
 * dependencies; uses the global `fetch` available in Node 18+ and browsers.
 */

export type {
  AgentFunction,
  AgentJson,
  Chidori,
  DatePolicy,
  ExecOptions,
  HttpRequestOptions,
  HttpResponse,
  InputOptions,
  JsonObject,
  JsonSchema,
  MapSetSnapshotPolicy,
  MemoryAction,
  ParallelOptions,
  PromptOptions,
  PromptStreamType,
  RandomPolicy,
  RetryOptions,
  RuntimePolicyConfig,
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
export type SessionStatus = "running" | "completed" | "failed" | "paused";

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
  | { type: "done"; id: string; status: SessionStatus; output?: Json; error?: string };

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
 */
export class AgentClient {
  readonly baseUrl: string;

  constructor(baseUrl: string = "http://localhost:8080") {
    this.baseUrl = baseUrl.replace(/\/$/, "");
  }

  async health(): Promise<Json> {
    return (await this.getJSON("/health")) as Json;
  }

  /** Create a new session and run the agent with the given input. */
  async run(input: Json): Promise<Session> {
    const data = await this.postJSON("/sessions", { input });
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
    const resp = await fetch(`${this.baseUrl}/sessions/stream`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
      body: JSON.stringify({ input }),
    });
    if (!resp.ok || !resp.body) {
      throw new Error(`stream request failed: ${resp.status}`);
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
    );
  }

  private async getJSON(path: string): Promise<unknown> {
    const resp = await fetch(this.baseUrl + path);
    if (!resp.ok) throw await httpError(resp);
    return await resp.json();
  }

  private async postJSON(path: string, body: unknown): Promise<Record<string, unknown>> {
    const resp = await fetch(this.baseUrl + path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!resp.ok) throw await httpError(resp);
    return (await resp.json()) as Record<string, unknown>;
  }
}

async function httpError(resp: Response): Promise<Error> {
  const text = await resp.text().catch(() => "");
  return new Error(`HTTP ${resp.status}: ${text}`);
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
    if (event === "done") return { type: "done", ...(data as object) } as StreamEvent;
  } catch {
    return null;
  }
  return null;
}
