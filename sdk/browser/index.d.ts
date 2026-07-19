// Type declarations for @1kbirds/chidori-browser (see index.js).

/** JSON value as it crosses the journaled host boundary. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface LlmRequest {
  text: string;
  opts: { model?: string; system?: string; maxTokens?: number; [key: string]: Json | undefined } | null;
}

export type LlmHandler = (request: LlmRequest) => Promise<string | Json>;

export interface HostOptions {
  /** Serves `chidori.prompt()`. Defaults to `mockLlm()`. */
  llm?: LlmHandler;
  /** Serves `chidori.tool(name, kwargs)`; keyed by tool name. */
  tools?: Record<string, (kwargs: Json) => Json | Promise<Json>>;
  /**
   * Serves `chidori.input()`. Return the answer, or `undefined` to suspend
   * the run (save `agent.blob()` and restore later).
   */
  onInput?: (payload: { prompt: string; opts: Json }) => string | undefined | Promise<string | undefined>;
  /** Observes `chidori.log()` as it is journaled. */
  onLog?: (payload: { message: string; fields?: Json }) => void;
  /** Serves `chidori.signal(name)`; throw or omit to fail signal waits. */
  onSignal?: (payload: { name: Json; opts: Json }) => Json | Promise<Json>;
  /** Override for `chidori.fetch()` (defaults to the page's `fetch`). */
  fetchImpl?: typeof fetch;
}

export interface CompletedRun {
  status: 'completed';
  console: string[];
  /** Host effects performed live this pump (0 on a pure replay). */
  liveCalls: number;
}

export interface SuspendedRun {
  status: 'suspended';
  console: string[];
  liveCalls: number;
  pendingInput: { prompt: string; opts: Json };
}

export type RunResult = CompletedRun | SuspendedRun;

/** Journaled host-effect names installed into every bundle. */
export const EFFECTS: string[];

/** JS prelude that presents `chidori.*` over the journaled effect globals. */
export const PRELUDE: string;

export function mockLlm(replies?: Record<string, string>): LlmHandler;

export function anthropicLlm(cfg: {
  apiKey: string;
  model?: string;
  maxTokens?: number;
  baseUrl?: string;
  fetchImpl?: typeof fetch;
}): LlmHandler;

export function openaiCompatibleLlm(cfg: {
  baseUrl: string;
  apiKey?: string;
  model: string;
  headers?: Record<string, string>;
  fetchImpl?: typeof fetch;
}): LlmHandler;

export function openRouterLlm(cfg: {
  apiKey: string;
  /** OpenRouter model id (e.g. "anthropic/claude-sonnet-4.5"); defaults to "openrouter/auto". */
  model?: string;
  /** Populates OpenRouter's X-Title attribution header. */
  appName?: string;
  /** Populates OpenRouter's HTTP-Referer attribution header. */
  appUrl?: string;
  fetchImpl?: typeof fetch;
}): LlmHandler;

/**
 * Begin OpenRouter's PKCE login: stores a code verifier in sessionStorage and
 * navigates to the consent page (or returns the URL with `redirect: false`).
 */
export function startOpenRouterLogin(options?: {
  callbackUrl?: string;
  redirect?: boolean;
}): Promise<string>;

/**
 * Finish OpenRouter's PKCE login on the callback page: exchanges `?code=` for
 * an API key, scrubs it from the URL, and returns the key — or null when this
 * page load is not a login callback (safe to call unconditionally).
 */
export function completeOpenRouterLogin(options?: {
  fetchImpl?: typeof fetch;
}): Promise<string | null>;

export function saveRun(key: string, blob: Uint8Array): void;
export function loadRun(key: string): Uint8Array | null;

/**
 * A client-side chidori agent: the wasm runtime plus this page's host
 * implementations. `wasm` is the initialized module from
 * `crates/chidori-wasm` (`import init, * as wasm from '.../chidori_wasm.js'`).
 */
export class BrowserAgent {
  static start(wasm: unknown, options: { source: string; filename?: string } & HostOptions): BrowserAgent;
  static restore(wasm: unknown, blob: Uint8Array, host?: HostOptions): BrowserAgent;
  run(): Promise<RunResult>;
  console(): string[];
  blob(): Uint8Array;
}
