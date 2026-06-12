/**
 * Type declarations for TypeScript agents executed inside the Chidori runtime.
 *
 * These are authoring-time types only. The runtime injects the concrete
 * `chidori` host object when it evaluates an agent or tool module.
 */

export type AgentJson =
  | null
  | boolean
  | number
  | string
  | AgentJson[]
  | { [key: string]: AgentJson };

export type JsonObject = { [key: string]: AgentJson };

export interface JsonSchema {
  type?: "object" | "array" | "string" | "number" | "integer" | "boolean" | "null";
  description?: string;
  properties?: Record<string, JsonSchema>;
  items?: JsonSchema;
  required?: string[];
  enum?: AgentJson[];
  default?: AgentJson;
  additionalProperties?: boolean | JsonSchema;
  [keyword: string]: unknown;
}

export interface ToolDefinition {
  name: string;
  description: string;
  parameters: JsonSchema & { type: "object" };
}

export type PromptStreamType = "progress" | "draft" | "subagent" | "final" | (string & {});

/** Provider prompt-cache lifetime for a cached prefix. */
export type CacheTtl = "5m" | "1h";

export interface PromptOptions {
  type?: PromptStreamType;
  system?: string;
  model?: string;
  maxTokens?: number;
  max_tokens?: number;
  maxTurns?: number;
  max_turns?: number;
  temperature?: number;
  tools?: string[];
  format?: "json" | (string & {});
  stream?: boolean;
  /**
   * Prompt-cache posture. Defaults to on (`"5m"`): the runtime marks the
   * stable request head (system, tools, conversation prefix) so providers
   * bill repeated prefixes at the cached rate. `false` disables marking for
   * this call; `"1h"` (or `{ ttl: "1h" }`) requests the extended TTL.
   * Caching never changes a response — only how it is billed.
   */
  cache?: boolean | CacheTtl | { ttl?: CacheTtl };
}

/** Structured response from `Context.respond()` — mirrors the provider turn. */
export interface LlmResponseJson {
  content: string;
  blocks: AgentJson[];
  tool_calls: { id: string; name: string; input: AgentJson }[];
  stop_reason: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
}

/**
 * An immutable, content-addressed, turn-structured prompt context.
 *
 * Builder methods return a NEW context that structurally shares this one's
 * segments — `base.user("a")` and `base.user("b")` are independent and share
 * `base`'s prefix — which keeps cache prefixes stable and makes forks cheap.
 * Only `prompt()` / `respond()` perform a durable host call.
 *
 * ```ts
 * const base = chidori.context()
 *   .system("You are a policy analyst.")
 *   .doc("corpus", corpusText)
 *   .cacheBreakpoint("1h");
 * let ctx = base;
 * for (const q of questions) {
 *   ctx = ctx.user(q);
 *   const { text, context } = await ctx.prompt();
 *   ctx = context; // assistant turn appended, prefix still shared
 * }
 * ```
 */
export interface Context {
  system(text: string): Context;
  /** Expose registered tools (by name, resolved like `prompt({ tools })`). */
  tools(names: string[]): Context;
  /** A large stable reference block, labelled for the trace. */
  doc(label: string, text: string): Context;
  user(text: string): Context;
  assistant(text: string): Context;
  toolResult(id: string, content: string, isError?: boolean): Context;
  /**
   * Freeze everything appended so far as a cacheable prefix (one provider
   * cache breakpoint). Providers cap breakpoints, so marks are coalesced —
   * latest wins. Most authors never need this: stable heads are auto-marked.
   */
  cacheBreakpoint(ttl?: CacheTtl): Context;
  /** Send this context; returns the text and the context extended with the
   * assistant turn (including any internal tool-use exchange). */
  prompt(options?: PromptOptions): Promise<{ text: string; context: Context }>;
  /** Single structured turn for author-driven tool loops. */
  respond(options?: PromptOptions): Promise<{ response: LlmResponseJson; context: Context }>;
  /** Stable content hash of the request this context would assemble. */
  digest(options?: PromptOptions): string;
  /** Rough local token estimate for window budgeting. */
  estimateTokens(): number;
}

export interface InputOptions {
  type?: string;
  default?: string;
  choices?: string[];
}

/**
 * Who delivered a signal. `kind` distinguishes a human participant from a peer
 * agent; `id` is the participant identity; `runId` is set when an agent sends
 * (its own run id), so agent-to-agent coordination is attributable in the trace.
 */
export interface SignalSender {
  kind: "human" | "agent";
  id: string;
  runId?: string;
}

/**
 * A named message delivered into a run mid-flight (`docs/signals.md` §6.1). The
 * inverse of `input()`: an outside party (human or agent) pushes
 * `{ name, payload, from }` at an agent-declared listen point. Every signal is
 * recorded in the call log, so the multiplayer session replays deterministically.
 */
export interface Signal<T = AgentJson> {
  name: string;
  payload: T;
  from: SignalSender;
}

export interface SignalOptions {
  /** Phase 2 (future): resolve to a timeout sentinel instead of waiting forever. */
  timeoutMs?: number;
}

export interface ParallelOptions {
  concurrency?: number;
}

/**
 * One `chidori.branch` variant (`docs/branching-execution.md` §6.1). A branch
 * runs its own continuation source module from the parent's anchored state —
 * not a re-run of the parent agent — so `source` is required.
 */
export interface BranchVariant {
  /** Branch label, shown in outcomes and the trace. Defaults to `branch-<k>`. */
  label?: string;
  /** Branch source module path, resolved like `callAgent` paths. */
  source: string;
  /** State handed to the branch as its run input. Defaults to `{}`. */
  input?: AgentJson;
}

export type BranchStatus = "completed" | "paused" | "failed";

/** The result of one branch sub-run, returned for comparison (not merged). */
export interface BranchOutcome<T extends AgentJson = AgentJson> {
  label: string;
  /**
   * `<parent run id>-op<branch seq>-branch-<k>` — identifies the branch
   * sub-run, including for out-of-band `chidori branch-resume` /
   * `branch-rerun` against its persisted store.
   */
  branchId: string;
  status: BranchStatus;
  /** The branch's output, when `status` is `"completed"`. */
  output?: T;
  /** What the branch is waiting on, when `status` is `"paused"`. */
  pendingPrompt?: string;
  /** The failure message, when `status` is `"failed"`. */
  error?: string;
}

export interface BranchOptions {
  /**
   * Maximum branches running live at once (cost cap). Defaults to 1 —
   * sequential. Higher values run variants in concurrent waves; outcome
   * order always follows variant order.
   */
  concurrency?: number;
}

export interface RetryOptions {
  attempts?: number;
  delayMs?: number;
  backoff?: "fixed" | "exponential";
}

export interface TryCallResult<T> {
  ok: boolean;
  value?: T;
  error?: string;
}

export interface HttpRequestOptions {
  method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD";
  headers?: Record<string, string>;
  query?: Record<string, string | number | boolean>;
  body?: AgentJson | string;
  timeoutMs?: number;
}

export interface HttpResponse {
  status: number;
  headers: Record<string, string>;
  body: AgentJson | string | null;
}

export interface TemplateOptions {
  source?: "file" | "inline";
}

export type MemoryAction = "get" | "set" | "delete" | "list" | "clear";

export type WorkspaceFileStatus = "complete" | "writing" | "failed";

export interface WorkspaceEntry {
  path: string;
  status: WorkspaceFileStatus;
  sha256: string;
  bytes: number;
  language?: string | null;
  attempt?: number | null;
  updatedAt?: string | null;
}

export interface WorkspaceListOptions {
  completeOnly?: boolean;
}

export interface WorkspaceWriteOptions {
  language?: string;
}

export interface WorkspaceHost {
  list(options?: WorkspaceListOptions): Promise<WorkspaceEntry[]>;
  read(path: string): Promise<string>;
  write(path: string, content: string, options?: WorkspaceWriteOptions): Promise<WorkspaceEntry>;
  delete(path: string, reason?: string): Promise<void>;
  remove(path: string, reason?: string): Promise<void>;
  manifest(): Promise<AgentJson>;
}

export type TypeScriptImportPolicy = "none" | "relative" | "project";
export type DatePolicy = "disabled" | "fixed" | "host";
export type RandomPolicy = "disabled" | "seeded" | "host";
export type MapSetSnapshotPolicy = "reject" | "serialize";

export interface RuntimePolicyConfig {
  typescript?: {
    imports?: TypeScriptImportPolicy;
  };
  runtime?: {
    date?: DatePolicy;
    random?: RandomPolicy;
  };
  snapshot?: {
    mapsSets?: MapSetSnapshotPolicy;
  };
}

export interface Chidori {
  workspace: WorkspaceHost;
  /** Start an immutable multi-turn prompt context (optionally seeded). */
  context(seed?: { system?: string; tools?: string[] }): Context;
  prompt(text: string, options?: PromptOptions): Promise<string>;
  input(message: string, options?: InputOptions): Promise<string>;
  /**
   * Pause at a named listen point until a matching signal is delivered (or one
   * is already queued in the durable mailbox), then resolve to
   * `{ name, payload, from }`. The inverse of `input()`: the run idles cheaply
   * on disk and an outside party delivers via `POST /sessions/{id}/signal`.
   */
  signal<T = AgentJson>(name: string, options?: SignalOptions): Promise<Signal<T>>;
  /**
   * Non-blocking: consume a queued signal of this name if present, else resolve
   * to `null`. Records the result (value or null) at this seq so replay is
   * deterministic.
   */
  pollSignal<T = AgentJson>(name: string): Promise<Signal<T> | null>;
  callAgent<TInput extends AgentJson = JsonObject, TOutput extends AgentJson = AgentJson>(
    path: string,
    input?: TInput,
  ): Promise<TOutput>;
  /**
   * Fork the run into one sub-run per variant from the current anchored state
   * (the VFS plus each variant's explicit `input`), run each variant's own
   * source module, and return every outcome so the agent can compare and pick.
   * The whole fan-out is one recorded durable call: a replay of this run
   * returns the outcomes from cache without re-running the branches.
   */
  branch<T extends AgentJson = AgentJson>(
    variants: BranchVariant[],
    options?: BranchOptions,
  ): Promise<BranchOutcome<T>[]>;
  tool<TArgs extends JsonObject = JsonObject, TResult extends AgentJson = AgentJson>(
    name: string,
    args?: TArgs,
  ): Promise<TResult>;
  parallel<TTasks extends readonly (() => Promise<unknown>)[]>(
    tasks: TTasks,
    options?: ParallelOptions,
  ): Promise<{ [Index in keyof TTasks]: Awaited<ReturnType<TTasks[Index]>> }>;
  retry<T>(fn: () => Promise<T>, options?: RetryOptions): Promise<T>;
  tryCall<T>(fn: () => Promise<T>): Promise<TryCallResult<T>>;
  http(url: string, options?: HttpRequestOptions): Promise<HttpResponse>;
  template(pathOrText: string, vars?: JsonObject, options?: TemplateOptions): Promise<string>;
  log(message: string, fields?: JsonObject): Promise<void>;
  memory<T extends AgentJson = AgentJson>(
    action: MemoryAction,
    key?: string,
    value?: T,
    options?: JsonObject,
  ): Promise<T | AgentJson[] | null>;
  checkpoint(label?: string, data?: AgentJson): Promise<void>;
}

export type AgentFunction<TInput extends AgentJson = JsonObject, TOutput extends AgentJson = AgentJson> = (
  input: TInput,
) => TOutput | Promise<TOutput>;

export type ToolFunction<TArgs extends JsonObject = JsonObject, TResult extends AgentJson = AgentJson> = (
  args: TArgs,
) => TResult | Promise<TResult>;

/**
 * The chidori host object — the durable surface your agents and tools call
 * (`chidori.log`, `chidori.prompt`, `chidori.tool`, `chidori.input`, …).
 *
 * Import it for typed access; the runtime strips this import and supplies the
 * real object at execution time, so there's no actual module dependency (and no
 * need for a `(input, chidori)` second parameter):
 *
 * ```ts
 * import { chidori, run } from "chidori";
 * run(async (input: { topic: string }) => {
 *   await chidori.log("starting", { topic: input.topic });
 *   return { ok: true };
 * });
 * ```
 *
 * Accessing it from a plain import outside the runtime throws.
 */
export const chidori: Chidori = new Proxy({} as Chidori, {
  get(_target, prop) {
    throw new Error(
      `chidori.${String(prop)} is only available inside the chidori runtime; ` +
        `this import is replaced when an agent runs under chidori.`,
    );
  },
});

/**
 * Define the agent entrypoint. Call it once at the top level of an agent module
 * with your handler; the runtime invokes the handler with the run input and
 * uses its return value as the output. This replaces the old "export a function
 * named `agent`" convention.
 *
 * ```ts
 * import { run } from "chidori";
 * run(async (input) => ({ greeting: `hello ${input.name}` }));
 * ```
 */
export function run<TInput extends AgentJson = JsonObject, TOutput extends AgentJson = AgentJson>(
  handler: AgentFunction<TInput, TOutput>,
): void {
  void handler;
  throw new Error(
    "run() is only available inside the chidori runtime; this import is " +
      "replaced when an agent runs under chidori.",
  );
}
