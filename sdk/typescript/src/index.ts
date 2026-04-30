/**
 * chidori TypeScript SDK — HTTP client for a running `chidori serve`
 * instance. Mirrors the Python SDK (`sdk/python/chidori`). Zero runtime
 * dependencies; uses the global `fetch` available in Node 18+ and browsers.
 */

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

/**
 * A saved checkpoint — session id, input, and the full call log. Enough to
 * replay a run without re-executing its side effects.
 */
export class Checkpoint {
  constructor(
    public readonly sessionId: string,
    public readonly input: Json,
    public readonly callLog: CallRecord[],
  ) {}

  /** Serialise to JSON. Pairs with `Checkpoint.fromJSON`. */
  toJSON(): { session_id: string; input: Json; call_log: CallRecord[] } {
    return {
      session_id: this.sessionId,
      input: this.input,
      call_log: this.callLog,
    };
  }

  static fromJSON(data: {
    session_id: string;
    input: Json;
    call_log: CallRecord[];
  }): Checkpoint {
    return new Checkpoint(data.session_id, data.input, data.call_log);
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
  ) {}

  get ok(): boolean {
    return this.status === "completed";
  }

  /**
   * Fetch the full call log from the server (if not already loaded) and
   * wrap it in a Checkpoint suitable for saving / later replay.
   */
  async checkpoint(): Promise<Checkpoint> {
    if (this.callLog.length === 0 && this.client) {
      const data = await this.client.getCheckpoint(this.id);
      this.callLog = data.call_log;
    }
    return new Checkpoint(this.id, this.input, this.callLog);
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

  /** Fetch the full call log for a session. */
  async getCheckpoint(id: string): Promise<{ call_log: CallRecord[] }> {
    return (await this.getJSON(`/sessions/${id}/checkpoint`)) as {
      call_log: CallRecord[];
    };
  }

  /**
   * Stream an agent run: yields one event per host function call plus a
   * final `done` event. Uses the server's `POST /sessions/stream` SSE
   * endpoint. Requires an environment with streaming `fetch` (Node 18+,
   * modern browsers).
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
    if (event === "done") return { type: "done", ...(data as object) } as StreamEvent;
  } catch {
    return null;
  }
  return null;
}
