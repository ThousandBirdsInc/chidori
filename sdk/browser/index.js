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
 * @param {{ apiKey: string, model?: string, maxTokens?: number, baseUrl?: string,
 *           fetchImpl?: typeof fetch }} cfg
 */
export function anthropicLlm({ apiKey, model = 'claude-sonnet-5', maxTokens = 1024, baseUrl = 'https://api.anthropic.com', fetchImpl }) {
  const doFetch = fetchImpl ?? fetch;
  return async ({ text, opts }) => {
    const res = await doFetch(`${baseUrl}/v1/messages`, {
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
 * @param {{ baseUrl: string, apiKey?: string, model: string,
 *           headers?: Record<string, string>, fetchImpl?: typeof fetch }} cfg
 */
export function openaiCompatibleLlm({ baseUrl, apiKey, model, headers, fetchImpl }) {
  const doFetch = fetchImpl ?? fetch;
  return async ({ text, opts }) => {
    const res = await doFetch(`${baseUrl.replace(/\/$/, '')}/chat/completions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
        ...headers,
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

/**
 * Call OpenRouter from the browser (OpenRouter's API is CORS-enabled for
 * client-side use). Obtain the key from user input, or — better for
 * client-side apps — via the PKCE login pair
 * {@link startOpenRouterLogin} / {@link completeOpenRouterLogin}, so users
 * authenticate on openrouter.ai and never paste a key at all.
 * `appName`/`appUrl` populate OpenRouter's optional attribution headers.
 * @param {{ apiKey: string, model?: string, appName?: string, appUrl?: string,
 *           fetchImpl?: typeof fetch }} cfg
 */
export function openRouterLlm({ apiKey, model = 'openrouter/auto', appName, appUrl, fetchImpl }) {
  return openaiCompatibleLlm({
    baseUrl: 'https://openrouter.ai/api/v1',
    apiKey,
    model,
    headers: {
      ...(appUrl ? { 'HTTP-Referer': appUrl } : {}),
      ...(appName ? { 'X-Title': appName } : {}),
    },
    fetchImpl,
  });
}

const OPENROUTER_VERIFIER_KEY = 'chidori-openrouter-verifier';

/** @private RFC 4648 base64url, no padding — the PKCE alphabet. */
function base64url(bytes) {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/**
 * Begin OpenRouter's PKCE login: generates a code verifier (kept in
 * sessionStorage), and navigates to openrouter.ai's consent page, which
 * redirects back to `callbackUrl` with a `?code=` parameter. Call
 * {@link completeOpenRouterLogin} on the callback page to obtain the API key.
 * Pass `redirect: false` to get the URL back (e.g. for a link) instead of
 * navigating.
 * @param {{ callbackUrl?: string, redirect?: boolean }} [options]
 * @returns {Promise<string>} the authorization URL
 */
export async function startOpenRouterLogin({ callbackUrl = location.href, redirect = true } = {}) {
  const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier));
  const challenge = base64url(new Uint8Array(digest));
  sessionStorage.setItem(OPENROUTER_VERIFIER_KEY, verifier);
  const url = 'https://openrouter.ai/auth' +
    `?callback_url=${encodeURIComponent(callbackUrl)}` +
    `&code_challenge=${challenge}&code_challenge_method=S256`;
  if (redirect) location.assign(url);
  return url;
}

/**
 * Finish OpenRouter's PKCE login on the callback page: exchanges the `?code=`
 * query parameter (plus the stored verifier) for a user-controlled API key,
 * scrubs the code from the address bar, and returns the key — ready to hand
 * to {@link openRouterLlm}. Returns null when the URL carries no code (i.e.
 * this page load is not a login callback), so it is safe to call
 * unconditionally at startup.
 * @param {{ fetchImpl?: typeof fetch }} [options]
 * @returns {Promise<string | null>}
 */
export async function completeOpenRouterLogin({ fetchImpl } = {}) {
  const here = new URL(location.href);
  const code = here.searchParams.get('code');
  if (!code) return null;
  const verifier = sessionStorage.getItem(OPENROUTER_VERIFIER_KEY);
  const doFetch = fetchImpl ?? fetch;
  const res = await doFetch('https://openrouter.ai/api/v1/auth/keys', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      code,
      ...(verifier ? { code_verifier: verifier, code_challenge_method: 'S256' } : {}),
    }),
  });
  if (!res.ok) throw new Error(`openrouter key exchange: ${res.status} ${await res.text()}`);
  const { key } = await res.json();
  sessionStorage.removeItem(OPENROUTER_VERIFIER_KEY);
  // Scrub the one-time code so a reload doesn't attempt a second exchange.
  here.searchParams.delete('code');
  history.replaceState(null, '', here);
  return key;
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
