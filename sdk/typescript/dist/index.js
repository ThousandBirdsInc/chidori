/**
 * app-agent TypeScript SDK — HTTP client for a running `app-agent serve`
 * instance. Mirrors the Python SDK (`sdk/python/app_agent`). Zero runtime
 * dependencies; uses the global `fetch` available in Node 18+ and browsers.
 */
/**
 * A saved checkpoint — session id, input, and the full call log. Enough to
 * replay a run without re-executing its side effects.
 */
export class Checkpoint {
    sessionId;
    input;
    callLog;
    constructor(sessionId, input, callLog) {
        this.sessionId = sessionId;
        this.input = input;
        this.callLog = callLog;
    }
    /** Serialise to JSON. Pairs with `Checkpoint.fromJSON`. */
    toJSON() {
        return {
            session_id: this.sessionId,
            input: this.input,
            call_log: this.callLog,
        };
    }
    static fromJSON(data) {
        return new Checkpoint(data.session_id, data.input, data.call_log);
    }
}
/** One execution of an agent — result + call log + status. */
export class Session {
    id;
    status;
    input;
    output;
    error;
    callLog;
    pendingPrompt;
    client;
    constructor(id, status, input, output = null, error = null, callLog = [], pendingPrompt = null, client = null) {
        this.id = id;
        this.status = status;
        this.input = input;
        this.output = output;
        this.error = error;
        this.callLog = callLog;
        this.pendingPrompt = pendingPrompt;
        this.client = client;
    }
    get ok() {
        return this.status === "completed";
    }
    /**
     * Fetch the full call log from the server (if not already loaded) and
     * wrap it in a Checkpoint suitable for saving / later replay.
     */
    async checkpoint() {
        if (this.callLog.length === 0 && this.client) {
            const data = await this.client.getCheckpoint(this.id);
            this.callLog = data.call_log;
        }
        return new Checkpoint(this.id, this.input, this.callLog);
    }
    /** Replay this session through the server; same inputs, cached results. */
    async replay() {
        if (!this.client) {
            throw new Error("Session has no client bound; use client.replay()");
        }
        const cp = await this.checkpoint();
        return this.client.replay(cp);
    }
}
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
export class AgentClient {
    baseUrl;
    constructor(baseUrl = "http://localhost:8080") {
        this.baseUrl = baseUrl.replace(/\/$/, "");
    }
    async health() {
        return (await this.getJSON("/health"));
    }
    /** Create a new session and run the agent with the given input. */
    async run(input) {
        const data = await this.postJSON("/sessions", { input });
        return this.sessionFrom(data, input);
    }
    /** Replay an agent from a saved checkpoint. */
    async replay(checkpoint) {
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
    async resume(sessionId, response) {
        const data = await this.postJSON(`/sessions/${sessionId}/resume`, {
            response,
        });
        return this.sessionFrom(data, data.input ?? null);
    }
    /** Fetch an existing session by id. */
    async getSession(id) {
        const data = (await this.getJSON(`/sessions/${id}`));
        return this.sessionFrom(data, data.input ?? null);
    }
    /** List all sessions. Returns the raw summaries. */
    async listSessions() {
        const data = (await this.getJSON("/sessions"));
        return data.sessions;
    }
    /** Fetch the full call log for a session. */
    async getCheckpoint(id) {
        return (await this.getJSON(`/sessions/${id}/checkpoint`));
    }
    /**
     * Stream an agent run: yields one event per host function call plus a
     * final `done` event. Uses the server's `POST /sessions/stream` SSE
     * endpoint. Requires an environment with streaming `fetch` (Node 18+,
     * modern browsers).
     */
    async *stream(input) {
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
            if (done)
                break;
            buffer += decoder.decode(value, { stream: true });
            let idx;
            while ((idx = buffer.indexOf("\n\n")) !== -1) {
                const frame = buffer.slice(0, idx);
                buffer = buffer.slice(idx + 2);
                const parsed = parseSseFrame(frame);
                if (parsed)
                    yield parsed;
            }
        }
    }
    // -- internals ----------------------------------------------------------
    sessionFrom(data, input) {
        return new Session(data.id, data.status ?? "failed", input, data.output ?? null, data.error ?? null, data.call_log ?? [], data.pending_prompt ?? null, this);
    }
    async getJSON(path) {
        const resp = await fetch(this.baseUrl + path);
        if (!resp.ok)
            throw await httpError(resp);
        return await resp.json();
    }
    async postJSON(path, body) {
        const resp = await fetch(this.baseUrl + path, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
        });
        if (!resp.ok)
            throw await httpError(resp);
        return (await resp.json());
    }
}
async function httpError(resp) {
    const text = await resp.text().catch(() => "");
    return new Error(`HTTP ${resp.status}: ${text}`);
}
function parseSseFrame(frame) {
    let event = "message";
    const dataLines = [];
    for (const line of frame.split("\n")) {
        if (line.startsWith("event:"))
            event = line.slice(6).trim();
        else if (line.startsWith("data:"))
            dataLines.push(line.slice(5).trim());
    }
    if (dataLines.length === 0)
        return null;
    try {
        const data = JSON.parse(dataLines.join("\n"));
        if (event === "call")
            return { type: "call", record: data };
        if (event === "done")
            return { type: "done", ...data };
    }
    catch {
        return null;
    }
    return null;
}
