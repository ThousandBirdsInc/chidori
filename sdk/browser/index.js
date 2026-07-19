// @1kbirds/chidori-browser — client-side chidori agents on the wasm engine.
//
// The wasm module (crates/chidori-wasm) carries the engine and the durable
// replay runtime; this package is the browser *host*: it presents the
// `chidori.*` agent API on top of the runtime's journaled host effects and
// implements those effects with browser primitives (fetch, timers, callbacks).
// Every effect result is journaled, so any run — including one suspended at
// `chidori.input()` — can be saved, restored in a fresh tab, resumed, and
// replayed offline with byte-identical output.
//
// Shipped as plain ESM with JSDoc types (see index.d.ts): usable directly
// from a <script type="module"> on a docs page, no bundler required.

/**
 * Journaled host-effect names, matching the native runtime's naming where an
 * equivalent exists (prompt/input/tool/log/signal), so journals stay
 * conceptually aligned across hosts.
 */
export const EFFECTS = [
  'prompt',
  'input',
  'tool',
  'log',
  'http_fetch',
  'sleep',
  'now',
  'random',
  'signal',
];

/**
 * Prepended to every bundle: presents the `chidori.*` API over the journaled
 * effect globals the runtime installs. Payload shapes mirror the native host
 * (`prompt` → `{text, opts}`, `input` → `{prompt, opts}`, `tool` →
 * `{name, kwargs}`, `log` → `{message, fields}`).
 */
export const PRELUDE = `const chidori = globalThis.chidori = {
  prompt: (text, opts) => prompt({ text: String(text), opts: opts ?? null }),
  input: (message, opts) => input({ prompt: String(message), opts: opts ?? null }),
  tool: (name, kwargs) => tool({ name: String(name), kwargs: kwargs ?? null }),
  log: (message, fields) => log(fields === undefined
    ? { message: String(message) }
    : { message: String(message), fields }),
  fetch: (url, opts) => http_fetch({ url: String(url), opts: opts ?? null }),
  sleep: (ms) => sleep({ ms: Number(ms) }),
  now: () => now({}),
  random: () => random({}),
  signal: (name, opts) => signal({ name, opts: opts ?? null }),
  step: (fn) => durableStep(fn),
};
`;

/**
 * A deterministic stand-in LLM for docs demos and tests: echoes the prompt,
 * or answers from a `{substring: reply}` table.
 * @param {Record<string, string>} [replies]
 */
export function mockLlm(replies) {
  return async ({ text }) => {
    if (replies) {
      for (const [needle, reply] of Object.entries(replies)) {
        if (text.includes(needle)) return reply;
      }
    }
    return `[mock] ${text}`;
  };
}

/**
 * Call the Anthropic Messages API directly from the browser. Requires a key
 * the user supplied to the page — never embed one in shipped code. Browser
 * calls are enabled via the `anthropic-dangerous-direct-browser-access`
 * header (Anthropic's own opt-in for client-side use).
 * @param {{ apiKey: string, model?: string, maxTokens?: number, baseUrl?: string }} cfg
 */
export function anthropicLlm({ apiKey, model = 'claude-sonnet-5', maxTokens = 1024, baseUrl = 'https://api.anthropic.com' }) {
  return async ({ text, opts }) => {
    const res = await fetch(`${baseUrl}/v1/messages`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        'x-api-key': apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-dangerous-direct-browser-access': 'true',
      },
      body: JSON.stringify({
        model: opts?.model ?? model,
        max_tokens: opts?.maxTokens ?? maxTokens,
        ...(opts?.system ? { system: opts.system } : {}),
        messages: [{ role: 'user', content: text }],
      }),
    });
    if (!res.ok) throw new Error(`anthropic: ${res.status} ${await res.text()}`);
    const body = await res.json();
    return body.content?.[0]?.text ?? '';
  };
}

/**
 * Call any OpenAI-compatible chat-completions endpoint (a LiteLLM proxy, a
 * local server, etc.).
 * @param {{ baseUrl: string, apiKey?: string, model: string }} cfg
 */
export function openaiCompatibleLlm({ baseUrl, apiKey, model }) {
  return async ({ text, opts }) => {
    const res = await fetch(`${baseUrl.replace(/\/$/, '')}/chat/completions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
      },
      body: JSON.stringify({
        model: opts?.model ?? model,
        messages: [
          ...(opts?.system ? [{ role: 'system', content: opts.system }] : []),
          { role: 'user', content: text },
        ],
      }),
    });
    if (!res.ok) throw new Error(`llm: ${res.status} ${await res.text()}`);
    const body = await res.json();
    return body.choices?.[0]?.message?.content ?? '';
  };
}

/** Persist a durable blob under a localStorage key. */
export function saveRun(key, blob) {
  localStorage.setItem(key, new TextDecoder().decode(blob));
}

/** Load a durable blob saved with {@link saveRun}; null when absent. */
export function loadRun(key) {
  const text = localStorage.getItem(key);
  return text === null ? null : new TextEncoder().encode(text);
}

/**
 * A client-side chidori agent: the wasm runtime plus this page's host
 * implementations. Construct with {@link BrowserAgent.start} (fresh run) or
 * {@link BrowserAgent.restore} (resume/replay a saved blob).
 */
export class BrowserAgent {
  /** @private */
  constructor(wasm, runtime, host) {
    this.wasm = wasm;
    this.runtime = runtime;
    this.host = host;
  }

  /**
   * Start a fresh recording of `source` (TypeScript or JavaScript — TS is
   * stripped by the wasm module's oxc transpiler).
   * @param {*} wasm - the initialized chidori_wasm module
   * @param {{ source: string, filename?: string } & HostOptions} options
   */
  static start(wasm, { source, filename = 'agent.ts', ...host }) {
    const js = wasm.stripTypes(source, filename);
    const runtime = new wasm.WasmRuntime(PRELUDE + js, EFFECTS);
    return new BrowserAgent(wasm, runtime, host);
  }

  /**
   * Restore a saved run from its durable blob. Journaled effects replay
   * without re-executing (no network, no reruns of anything
   * non-deterministic); the pump only surfaces work past the recorded
   * frontier. Restoring a *completed* run replays it end-to-end offline.
   * @param {*} wasm - the initialized chidori_wasm module
   * @param {Uint8Array} blob
   * @param {HostOptions} [host]
   */
  static restore(wasm, blob, host = {}) {
    const runtime = wasm.WasmRuntime.fromBlob(blob);
    return new BrowserAgent(wasm, runtime, host);
  }

  /**
   * Pump the agent: run until it completes or suspends. Each blocking host
   * effect is performed by this page's host implementations and journaled.
   * Resolves to
   * `{ status: 'completed', console, liveCalls }` or
   * `{ status: 'suspended', console, liveCalls, pendingInput: { prompt, opts } }`
   * (suspended = `chidori.input()` reached with no `onInput` answer; save
   * `this.blob()` and restore later).
   */
  async run() {
    let liveCalls = 0;
    for (;;) {
      const status = JSON.parse(this.runtime.runUntilBlocked());
      if (status.status === 'completed') {
        return { status: 'completed', console: this.runtime.consoleLines(), liveCalls };
      }
      const payload = status.args[0] ?? {};
      if (status.name === 'input') {
        const answer = await this.host.onInput?.(payload);
        if (answer === undefined) {
          return {
            status: 'suspended',
            console: this.runtime.consoleLines(),
            liveCalls,
            pendingInput: payload,
          };
        }
        liveCalls += 1;
        this.runtime.resolveOp(status.opId, JSON.stringify(String(answer)));
        continue;
      }
      liveCalls += 1;
      try {
        const result = await this.#effect(status.name, payload);
        this.runtime.resolveOp(status.opId, JSON.stringify(result ?? null));
      } catch (err) {
        this.runtime.rejectOp(status.opId, String(err?.message ?? err));
      }
    }
  }

  /** @private Perform one live host effect with this page's implementations. */
  async #effect(name, payload) {
    switch (name) {
      case 'prompt': {
        const llm = this.host.llm ?? mockLlm();
        return llm(payload);
      }
      case 'tool': {
        const impl = this.host.tools?.[payload.name];
        if (!impl) throw new Error(`no such tool: ${payload.name}`);
        return impl(payload.kwargs ?? {});
      }
      case 'log': {
        this.host.onLog?.(payload);
        return null;
      }
      case 'http_fetch': {
        const doFetch = this.host.fetchImpl ?? fetch;
        const res = await doFetch(payload.url, payload.opts ?? undefined);
        const text = await res.text();
        let json = null;
        try { json = JSON.parse(text); } catch { /* non-JSON body */ }
        return { status: res.status, ok: res.ok, text, json };
      }
      case 'sleep':
        await new Promise((r) => setTimeout(r, payload.ms));
        return null;
      case 'now':
        return Date.now();
      case 'random':
        return Math.random();
      case 'signal': {
        const answer = await this.host.onSignal?.(payload);
        if (answer === undefined) throw new Error(`no handler for signal: ${JSON.stringify(payload.name)}`);
        return answer;
      }
      default:
        throw new Error(`unknown host effect: ${name}`);
    }
  }

  /** Console output accumulated so far. */
  console() {
    return this.runtime.consoleLines();
  }

  /** The durable artifact (bundle + effects + journal). Save it anywhere. */
  blob() {
    return this.runtime.toBlob();
  }
}
