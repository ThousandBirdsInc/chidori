/**
 * app-agent TypeScript SDK — HTTP client for a running `app-agent serve`
 * instance. Mirrors the Python SDK (`sdk/python/app_agent`). Zero runtime
 * dependencies; uses the global `fetch` available in Node 18+ and browsers.
 */
/** JSON-serialisable value — what agents produce as output and accept as input. */
export type Json = null | boolean | number | string | Json[] | {
    [key: string]: Json;
};
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
    token_usage?: {
        input_tokens: number;
        output_tokens: number;
    };
    error?: string;
}
/**
 * A saved checkpoint — session id, input, and the full call log. Enough to
 * replay a run without re-executing its side effects.
 */
export declare class Checkpoint {
    readonly sessionId: string;
    readonly input: Json;
    readonly callLog: CallRecord[];
    constructor(sessionId: string, input: Json, callLog: CallRecord[]);
    /** Serialise to JSON. Pairs with `Checkpoint.fromJSON`. */
    toJSON(): {
        session_id: string;
        input: Json;
        call_log: CallRecord[];
    };
    static fromJSON(data: {
        session_id: string;
        input: Json;
        call_log: CallRecord[];
    }): Checkpoint;
}
/** One execution of an agent — result + call log + status. */
export declare class Session {
    readonly id: string;
    status: SessionStatus;
    readonly input: Json;
    output: Json | null;
    error: string | null;
    callLog: CallRecord[];
    pendingPrompt: string | null;
    private readonly client;
    constructor(id: string, status: SessionStatus, input: Json, output?: Json | null, error?: string | null, callLog?: CallRecord[], pendingPrompt?: string | null, client?: AgentClient | null);
    get ok(): boolean;
    /**
     * Fetch the full call log from the server (if not already loaded) and
     * wrap it in a Checkpoint suitable for saving / later replay.
     */
    checkpoint(): Promise<Checkpoint>;
    /** Replay this session through the server; same inputs, cached results. */
    replay(): Promise<Session>;
}
/** Stream event yielded by `AgentClient.stream`. */
export type StreamEvent = {
    type: "call";
    record: CallRecord;
} | {
    type: "done";
    id: string;
    status: SessionStatus;
    output?: Json;
    error?: string;
};
/**
 * HTTP client for an `app-agent serve` instance.
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
export declare class AgentClient {
    readonly baseUrl: string;
    constructor(baseUrl?: string);
    health(): Promise<Json>;
    /** Create a new session and run the agent with the given input. */
    run(input: Json): Promise<Session>;
    /** Replay an agent from a saved checkpoint. */
    replay(checkpoint: Checkpoint): Promise<Session>;
    /**
     * Supply a response to a paused `input()` call and continue the run.
     * The same session id advances to completed (or re-pauses on a later
     * `input()`).
     */
    resume(sessionId: string, response: string): Promise<Session>;
    /** Fetch an existing session by id. */
    getSession(id: string): Promise<Session>;
    /** List all sessions. Returns the raw summaries. */
    listSessions(): Promise<Array<{
        id: string;
        status: SessionStatus;
        error?: string;
    }>>;
    /** Fetch the full call log for a session. */
    getCheckpoint(id: string): Promise<{
        call_log: CallRecord[];
    }>;
    /**
     * Stream an agent run: yields one event per host function call plus a
     * final `done` event. Uses the server's `POST /sessions/stream` SSE
     * endpoint. Requires an environment with streaming `fetch` (Node 18+,
     * modern browsers).
     */
    stream(input: Json): AsyncGenerator<StreamEvent, void, void>;
    private sessionFrom;
    private getJSON;
    private postJSON;
}
