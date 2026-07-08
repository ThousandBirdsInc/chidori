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
  maxTurns?: number;
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
  toolCalls: { id: string; name: string; input: AgentJson }[];
  stopReason: string;
  inputTokens: number;
  outputTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
}

/** Options for `Context.compact()` — explicit, opt-in window compaction. */
export interface CompactOptions {
  /** How many of the newest conversation turns to keep verbatim (default 2). */
  keepTurns?: number;
  /**
   * Skip compaction (pure no-op, no host call) while `estimateTokens()` is at
   * or under this budget — lets a loop call `compact()` unconditionally.
   */
  budgetTokens?: number;
  /** Model for the summarization prompt (defaults like `prompt()`). */
  model?: string;
  /** System instructions for the summarizer (a faithful-brief default). */
  instructions?: string;
  /** `maxTokens` for the summarization prompt. */
  maxTokens?: number;
  /** Cache posture for the summarization prompt (see `PromptOptions.cache`). */
  cache?: boolean | CacheTtl | { ttl?: CacheTtl };
  /** TTL of the fresh cache breakpoint placed on the summary (default "5m"). */
  ttl?: CacheTtl;
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
  /**
   * Summarize the older conversation turns into one durable summary segment
   * (via a recorded `prompt` host call, so it replays deterministically) and
   * return a new context: stable head + summary + fresh cache breakpoint +
   * the kept newest turns. Never automatic — compaction changes what the
   * model sees, so it is always an explicit author decision. Returns this
   * context unchanged (without a host call) when there is nothing to compact
   * or the context is within `budgetTokens`.
   */
  compact(options?: CompactOptions): Promise<Context>;
  /** Stable content hash of the request this context would assemble. */
  digest(options?: PromptOptions): string;
  /** Rough local token estimate for window budgeting. */
  estimateTokens(): number;
}

/** One recorded exchange in a {@link Conversation} transcript. */
export interface ConversationTurn {
  role: "user" | "assistant";
  text: string;
}

/** Options for `chidori.conversation()`. */
export interface ConversationOptions {
  /** System prompt — frozen once as the conversation's cacheable prefix. */
  system?: string;
  /** Tool names available on every turn (resolved like `prompt({ tools })`). */
  tools?: string[];
  /** Default stream label for each turn's prompt (default `"final"`). */
  type?: PromptStreamType;
  /** Default model for each turn (a per-turn override still wins). */
  model?: string;
  /** Default output token cap for each turn. */
  maxTokens?: number;
  /** Default sampling temperature for each turn. */
  temperature?: number;
  /** Default cache posture for each turn (see {@link PromptOptions.cache}). */
  cache?: boolean | CacheTtl | { ttl?: CacheTtl };
  /** TTL of the cache breakpoint frozen over the system/tools head. */
  cacheTtl?: CacheTtl;
  /**
   * Opt-in window management: when set, each turn first runs the same budgeted
   * `Context.compact()` — a pure no-op until the running tail exceeds budget,
   * then the older turns fold into one recorded summary segment.
   */
  compact?: CompactOptions;
}

/** Options for `Conversation.loop()` — an interactive `input()`-driven loop. */
export interface ConversationLoopOptions {
  /** Prompt shown to the human each turn (or a function of the turn index). */
  prompt?: string | ((turn: number) => string);
  /** Extra options forwarded to `chidori.input()` (defaults to `{ type: "message" }`). */
  inputOptions?: InputOptions;
  /** Words that end the loop, case-insensitive (default `["exit", "quit"]`). */
  exit?: string | string[];
  /** Hard cap on the number of exchanges before returning. */
  maxTurns?: number;
  /** Skip blank input lines instead of sending them (default `true`). */
  skipEmpty?: boolean;
  /** Per-turn prompt options applied to every `say()` in the loop. */
  turn?: PromptOptions;
  /** Called with the assistant reply (and the user message) after each turn. */
  onReply?: (reply: string, message: string) => void | Promise<void>;
  /** Return `true` after a turn to end the loop (checked after `onReply`). */
  until?: (message: string, reply: string) => boolean;
}

/**
 * A small stateful wrapper over {@link Context} for the most common shape — a
 * multi-turn chat assistant. It owns the running context (system + tools frozen
 * as a cacheable prefix) and threads each turn through it, so you write
 * `chat.say(message)` instead of re-plumbing `ctx = (await
 * ctx.user(message).prompt()).context` by hand. Every turn is still one durable
 * `prompt`/`respond` host call that replays for free.
 *
 * ```ts
 * const chat = chidori.conversation({ system: "You are concise." });
 * const a = await chat.say("Hi, who are you?");
 * const b = await chat.say("What can you help with?");
 * // or drive it interactively:
 * const transcript = await chat.loop({ prompt: "you>" });
 * ```
 */
export interface Conversation {
  /** The underlying immutable context, for dropping to the lower-level API. */
  readonly context: Context;
  /** Number of completed exchanges (user+assistant pairs) so far. */
  readonly length: number;
  /** The transcript so far as plain `{ role, text }` entries. */
  history(): ConversationTurn[];
  /** Send one user message; resolves to the assistant's reply text. */
  say(message: string, options?: PromptOptions): Promise<string>;
  /**
   * Like `say()`, but resolves to the structured response (`tool_calls`,
   * `blocks`) for author-driven tool loops. Append tool results with
   * `chat.context.toolResult(...)`, then call `say()` again.
   */
  respond(message: string, options?: PromptOptions): Promise<LlmResponseJson>;
  /**
   * Drive an interactive loop: read a human message via `chidori.input()`
   * (terminal stdin under `chidori run`, a paused session resume under `chidori
   * serve`), reply with `say()`, and repeat until the user types an exit word
   * or `until` returns true. Resolves to the full transcript.
   */
  loop(options?: ConversationLoopOptions): Promise<ConversationTurn[]>;
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
 * A named message delivered into a run mid-flight (`docs/signals.md`). The
 * inverse of `input()`: an outside party (human or agent) pushes
 * `{ name, payload, from }` at an agent-declared listen point. Every signal is
 * recorded in the call log, so the multiplayer session replays deterministically.
 */
export interface Signal<T = AgentJson> {
  name: string;
  payload: T;
  from: SignalSender;
  /** Never set on a delivered signal — the discriminant against {@link SignalTimeout}, so `if (result.timedOut)` narrows. */
  timedOut?: undefined;
}

export interface SignalOptions {
  /**
   * Resolve to a {@link SignalTimeout} sentinel after this many milliseconds
   * instead of waiting forever. The deadline is enforced by the supervising
   * server while the run idles; the recorded result (signal or sentinel)
   * replays deterministically. Discriminate with `"timedOut" in result`.
   */
  timeoutMs?: number;
}

/**
 * The sentinel a `timeoutMs` listen point resolves to when the deadline passes
 * with no matching delivery — a timeout resolves to this sentinel rather than
 * rejecting (`docs/signals.md`). `name` is the single awaited name, or `null`
 * for a multi-name fan-in listen.
 */
export interface SignalTimeout {
  name: string | null;
  payload: null;
  from: null;
  timedOut: true;
}

export interface ParallelOptions {
  concurrency?: number;
}

/**
 * One `chidori.branch` variant (`docs/branching-execution.md`). A branch
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

/**
 * What the runtime does when an actor iteration fails (`docs/actors.md`):
 * - `"never"` (default) — the failure is the actor's final outcome;
 * - `"clean"` — re-run the actor's module from scratch (fresh log, the
 *   spawn-time filesystem anchor, the original input);
 * - `"resume"` — replay the actor's accumulated call log with the trailing
 *   failed records stripped, so completed work returns from cache and the
 *   failing call itself retries live.
 */
export type ActorRestartStrategy = "never" | "clean" | "resume";

export interface SpawnActorOptions {
  /** Register the actor under a name for `actors.lookup`/`actors.send` addressing. */
  name?: string;
  /** Restart strategy applied when an iteration fails. Default `"never"`. */
  restart?: ActorRestartStrategy;
  /** Restart intensity cap (default 3). */
  maxRestarts?: number;
  /** Base delay between restarts, doubling per attempt (default 0). */
  backoffMs?: number;
  /**
   * How long the actor may sit at an empty-mailbox listen point with no
   * explicit `timeoutMs` before the runtime parks it as a `paused` outcome
   * (default 300 000 ms).
   */
  idleTimeoutMs?: number;
}

/** How an actor's supervision loop settled. */
export type ActorOutcomeStatus = "completed" | "failed" | "paused" | "stopped";

/** The settlement `actors.join()`/`actors.stop()` resolve to. */
export interface ActorOutcome<T extends AgentJson = AgentJson> {
  pid: string;
  status: ActorOutcomeStatus;
  /** The actor's return value, when `status` is `"completed"`. */
  output?: T;
  /** The final failure, when `status` is `"failed"`. */
  error?: string;
  /** What the actor is parked on, when `status` is `"paused"`. */
  pendingPrompt?: string;
  /** How many supervised restarts the actor consumed. */
  restarts: number;
}

/**
 * What a `join`/`stop` with `timeoutMs` resolves to when the deadline passes
 * before the actor settles: a snapshot, not a settlement — nothing merged,
 * join again later. Discriminate with `outcome.status === "running"`.
 */
export interface ActorStillRunning {
  pid: string;
  status: "running";
  restarts: number;
}

export interface ActorStatus {
  pid: string;
  status: ActorOutcomeStatus | "running" | "waiting" | "unknown";
  restarts?: number;
  /** Messages queued and not yet consumed. */
  mailbox?: number;
  /** The listen-point names a `"waiting"` actor is parked on. */
  waitingFor?: string[];
}

/** A message consumed from a mailbox. */
export interface ActorMessage<T = AgentJson> {
  name: string;
  payload: T;
  /** Sender identity: `id` is the sending actor's pid, or `"run"`. */
  from: SignalSender;
  /** Never set on a delivered message — the discriminant against {@link SignalTimeout}. */
  timedOut?: undefined;
}

export interface ReceiveOptions {
  /** Resolve to the `{ timedOut: true }` sentinel after this many ms. */
  timeoutMs?: number;
}

export interface JoinActorOptions {
  /**
   * Give up waiting after this many ms: the join resolves to
   * {@link ActorStillRunning} without settling the actor — join again later.
   */
  timeoutMs?: number;
}

/**
 * A spawned actor's handle: its address plus its lifecycle methods, so code
 * reads `worker.send(...)` / `worker.join()` instead of threading pid strings.
 * Pure sugar — every method is one recorded durable host call, identical to
 * the string-addressed forms on {@link Actors}.
 */
export interface ActorHandle {
  pid: string;
  /** The registered name, when the spawn options provided one. */
  name: string | null;
  send<T extends AgentJson = AgentJson>(name: string, payload?: T): Promise<{ delivered: boolean }>;
  join<T extends AgentJson = AgentJson>(): Promise<ActorOutcome<T>>;
  join<T extends AgentJson = AgentJson>(
    options: JoinActorOptions,
  ): Promise<ActorOutcome<T> | ActorStillRunning>;
  stop<T extends AgentJson = AgentJson>(): Promise<ActorOutcome<T>>;
  stop<T extends AgentJson = AgentJson>(
    options: JoinActorOptions,
  ): Promise<ActorOutcome<T> | ActorStillRunning>;
  status(): Promise<ActorStatus>;
}

/**
 * `chidori.actors` — supervised, message-passing actor processes
 * (`docs/actors.md`). `spawn` starts another agent module as a concurrent,
 * addressable sub-run with a durable mailbox and a restart policy; actors can
 * spawn their own children, forming a supervision tree (settling is
 * owner-only, `"parent"` addresses the owner, and supervisors reap their
 * children). Every method is a recorded durable call: a replay re-runs
 * nothing.
 */
export interface Actors {
  spawn<TInput extends AgentJson = JsonObject>(
    source: string,
    input?: TInput,
    options?: SpawnActorOptions,
  ): Promise<ActorHandle>;
  /**
   * Deliver a named message to a mailbox. `to` is a pid, a registered name,
   * or `"parent"` (the sender's spawner: its owning actor, or the run for a
   * top-level actor). Never blocks; `delivered` is false once the target has
   * settled.
   */
  send<T extends AgentJson = AgentJson>(
    to: string,
    name: string,
    payload?: T,
  ): Promise<{ delivered: boolean }>;
  /** String-addressed `join` — see {@link ActorHandle.join}. Owner-only. */
  join<T extends AgentJson = AgentJson>(target: string): Promise<ActorOutcome<T>>;
  join<T extends AgentJson = AgentJson>(
    target: string,
    options: JoinActorOptions,
  ): Promise<ActorOutcome<T> | ActorStillRunning>;
  /**
   * Request a cooperative stop (honored between iterations, at mailbox waits,
   * and during restart backoff — a live LLM/tool call finishes first), then
   * settle and merge exactly like `join`. Owner-only.
   */
  stop<T extends AgentJson = AgentJson>(target: string): Promise<ActorOutcome<T>>;
  stop<T extends AgentJson = AgentJson>(
    target: string,
    options: JoinActorOptions,
  ): Promise<ActorOutcome<T> | ActorStillRunning>;
  /** A durable snapshot of an actor's lifecycle. */
  status(target: string): Promise<ActorStatus>;
  /** Registry lookup: a handle for the actor registered under `name`, or `null`. */
  lookup(name: string): Promise<ActorHandle | null>;
}

/**
 * `chidori.memory` — the persistent, namespaced key-value store. Values are
 * JSON; `options.namespace` scopes keys (default `"default"`).
 */
export interface MemoryStore {
  set<T extends AgentJson = AgentJson>(key: string, value: T, options?: JsonObject): Promise<void>;
  get<T extends AgentJson = AgentJson>(key: string, options?: JsonObject): Promise<T | null>;
  delete(key: string, options?: JsonObject): Promise<void>;
  /** List entries, optionally filtered with `options.prefix`. */
  list(options?: JsonObject): Promise<AgentJson[]>;
  clear(options?: JsonObject): Promise<void>;
}

/**
 * `chidori.util` — in-VM control-flow helpers. Unlike everything else on the
 * host object these are pure JavaScript and record NOTHING: only the durable
 * calls made inside your callbacks appear in the journal.
 */
export interface ChidoriUtil {
  parallel<TTasks extends readonly (() => Promise<unknown>)[]>(
    tasks: TTasks,
    options?: ParallelOptions,
  ): Promise<{ [Index in keyof TTasks]: Awaited<ReturnType<TTasks[Index]>> }>;
  retry<T>(fn: () => Promise<T>, options?: RetryOptions): Promise<T>;
  tryCall<T>(fn: () => Promise<T>): Promise<TryCallResult<T>>;
}

/**
 * `chidori.appData` — the agent-run application-data store (generative-UI
 * runs). Writes and queries execute through the host boundary (the agent
 * never holds a credential) and are journaled like every other effect;
 * `params` bind server-side (`$1`, `$2`, …), never string-concatenated.
 */
export interface AppData {
  write(sql: string, params?: AgentJson[]): Promise<AgentJson>;
  query(sql: string, params?: AgentJson[]): Promise<AgentJson>;
}

export interface TryCallResult<T> {
  ok: boolean;
  value?: T;
  error?: string;
}

export interface TemplateOptions {
  source?: "file" | "inline";
}


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
  /**
   * Start a multi-turn chat assistant — a stateful wrapper over `context()`
   * that owns the running dialogue. Send turns with `chat.say(message)` or drive
   * an interactive `input()` loop with `chat.loop()`.
   */
  conversation(options?: ConversationOptions): Conversation;
  prompt(text: string, options?: PromptOptions): Promise<string>;
  input(message: string, options?: InputOptions): Promise<string>;
  /**
   * Pause at a named listen point until a matching signal is delivered (or one
   * is already queued in the durable mailbox), then resolve to
   * `{ name, payload, from }`. The inverse of `input()`: the run idles cheaply
   * on disk and an outside party delivers via `POST /sessions/{id}/signal`.
   * Pass an array for fan-in — the resolved signal's `name` says which fired;
   * pre-arrived candidates are consumed in delivery order. Reach for `signal`
   * when the delivery comes from OUTSIDE the process; use `receive` for actor
   * messages.
   */
  signal<T = AgentJson>(name: string | string[]): Promise<Signal<T>>;
  signal<T = AgentJson>(
    name: string | string[],
    options: SignalOptions,
  ): Promise<Signal<T> | SignalTimeout>;
  /**
   * Non-blocking: consume a queued signal of this name if present, else resolve
   * to `null`. Records the result (value or null) at this seq so replay is
   * deterministic.
   */
  pollSignal<T = AgentJson>(name: string): Promise<Signal<T> | null>;
  /**
   * Durable value checkpoint: run `fn` once and journal its JSON-serializable
   * result; on replay/resume the recorded value (or error) is returned without
   * re-running `fn`. Wrap expensive deterministic computation in a step so a
   * resumed run does not re-pay it. The callback must be pure, synchronous
   * compute — host effects (`chidori.*`), captured randomness, filesystem
   * writes, timers, and async callbacks are refused inside a step.
   */
  step<T extends AgentJson = AgentJson>(name: string, fn: () => T): Promise<T>;
  /**
   * Run another agent module inline and return its output — a synchronous
   * child call sharing this run's context and log. Of the three module
   * runners: need the answer inline → `callAgent`; comparing strategies →
   * `branch`; coordinating concurrent processes → `actors.spawn`.
   */
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
  /**
   * Supervised actor processes: spawn agent modules as concurrent,
   * addressable sub-runs with durable mailboxes, supervision trees, and
   * restart policies (`docs/actors.md`).
   */
  actors: Actors;
  /**
   * Blocking, in-place message consumption: inside an actor, drains the
   * actor's own mailbox; in the spawning run, drains messages actors sent to
   * `"parent"` (and any pre-queued external signals). Unlike `signal()` —
   * which pauses the whole run so an external party can resume it — `receive`
   * waits in place and is woken directly by in-process senders. Reach for
   * `receive` for actor messages; use `signal` for external deliveries.
   */
  receive<T = AgentJson>(names: string | string[]): Promise<ActorMessage<T>>;
  receive<T = AgentJson>(
    names: string | string[],
    options: ReceiveOptions,
  ): Promise<ActorMessage<T> | SignalTimeout>;
  tool<TArgs extends JsonObject = JsonObject, TResult extends AgentJson = AgentJson>(
    name: string,
    args?: TArgs,
  ): Promise<TResult>;
  /** In-VM control-flow helpers (`parallel`, `retry`, `tryCall`) — pure JS, never recorded. */
  util: ChidoriUtil;
  template(pathOrText: string, vars?: JsonObject, options?: TemplateOptions): Promise<string>;
  log(message: string, fields?: JsonObject): Promise<void>;
  /** The persistent, namespaced key-value store. */
  memory: MemoryStore;
  /** The journaled agent-run application-data store (generative-UI runs). */
  appData: AppData;
  /**
   * Record a labelled trace marker in the call log — an annotation, nothing
   * more. (The durable VALUE checkpoint is `chidori.step`.)
   */
  mark(label?: string, data?: AgentJson): Promise<void>;
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
 * import { chidori, run } from "chidori:agent";
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
 * import { run } from "chidori:agent";
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
