'use client';

/**
 * The page-side run host shared by the example-runner panel and the docs
 * VM's `chidori` CLI: everything a docs example can reach beyond the plain
 * prompt/tool/fetch surface of host.ts.
 *
 * - signals / receive / alarms: an interactive delivery surface (the panel
 *   shows a delivery form, the terminal a `signal>` prompt), plus per-run
 *   mailboxes so actor messages resolve `chidori.receive()` for real.
 * - actors and detached agents: each spawn starts a real nested run of the
 *   named source file (from the shared VM filesystem) on its own wasm
 *   runtime, concurrently on the page; mailboxes, join/stop/status, name
 *   registry, and `__chidori.down__` notifications are managed here.
 * - branches and callAgent: nested runs whose outcomes fold back into the
 *   parent as one journaled call, exactly like the native runtime's shape.
 * - workspace: backed by the same VFS the terminal browses — a file an
 *   example writes is the file `cat` prints.
 * - memory: localStorage-backed, so it genuinely persists across runs.
 * - appData: a micro subset of SQL over in-page tables.
 */

import type { Json } from '../../(home)/playground/brain';
import { buildHarnessSource, INTERNAL_TOOLS } from './harness';
import { decidePrompt, fetchWithSimulatedFallback, makeDocsTools } from './host';
import { dirname, resolvePath, type Vfs } from '../vm/vfs';

export interface SignalMessage {
  name: string | null;
  payload: Json;
  from: { kind: string; id: string; runId?: string } | null;
  timedOut?: boolean;
}

export interface SignalRequest {
  /** Names the run is listening for (`__alarm__` for alarms). */
  names: string[];
  timeoutMs: number | null;
  /** 'signal' waits on an outside party; 'receive' on actor messages. */
  mode: 'signal' | 'receive';
  /** Which run is waiting — the main run, or a named actor. */
  who: string;
  deliver: (msg: SignalMessage) => void;
  fireTimeout: () => void;
}

/** How the surrounding surface (panel or terminal) interacts with a run. */
export interface RunUi {
  /** A run paused at chidori.input(); resolve with the reader's answer. */
  askInput: (payload: { prompt: string; opts?: Json }, who: string) => Promise<string>;
  /** A run paused at a signal listen point; show a delivery surface. */
  waitSignal: (req: SignalRequest) => void;
  /** The signal wait was satisfied (message or timeout) — retire the UI. */
  signalDone: (req: SignalRequest) => void;
  /** Ask-posture approval gate (CLI); return 'yes' | 'all' | 'no'. */
  approve?: (what: string, target: string) => Promise<string>;
  /** Progress/annotation line. */
  note?: (text: string) => void;
  busy?: (text: string | null) => void;
  refresh?: () => void;
}

export interface EngineAssets {
  wasm: unknown;
  sdk: { BrowserAgent: { start: (wasm: unknown, opts: Record<string, unknown>) => AgentHandle; restore: (wasm: unknown, blob: Uint8Array, host: Record<string, unknown>) => AgentHandle } };
}

export interface RunResult {
  status: string;
  console: string[];
  liveCalls: number;
  pendingInput?: { prompt: string; opts?: Json };
}

export interface AgentHandle {
  run(): Promise<RunResult>;
  console(): string[];
  blob(): Uint8Array;
}

interface Mailbox {
  queue: SignalMessage[];
  waiters: { names: string[]; resolve: (msg: SignalMessage) => void }[];
}

interface ActorEntry {
  pid: string;
  scope: 'actors' | 'agents';
  name: string | null;
  source: string;
  status: 'running' | 'hibernating' | 'completed' | 'failed' | 'stopped';
  restarts: number;
  simulated: boolean;
  mailbox: Mailbox;
  waitingFor: string[] | null;
  output: Json;
  error: string | null;
  done: Promise<void>;
  stopFlag: { stopped: boolean };
}

const MEMORY_STORAGE = 'chidori-docs-memory';

const asObj = (v: Json): Record<string, Json> =>
  v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, Json>) : {};

function newMailbox(): Mailbox {
  return { queue: [], waiters: [] };
}

function deliverTo(box: Mailbox, msg: SignalMessage): boolean {
  const i = box.waiters.findIndex((w) => msg.name !== null && w.names.includes(msg.name));
  if (i >= 0) {
    const [waiter] = box.waiters.splice(i, 1);
    waiter.resolve(msg);
    return true;
  }
  box.queue.push(msg);
  return true;
}

function takeQueued(box: Mailbox, names: string[]): SignalMessage | null {
  const i = box.queue.findIndex((m) => m.name !== null && names.includes(m.name));
  return i >= 0 ? box.queue.splice(i, 1)[0] : null;
}

/** FNV-1a hex — the docs VM's stand-in for workspace sha256 fields. */
function contentHash(s: string): string {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0).toString(16).padStart(8, '0').repeat(4).slice(0, 16);
}

// ---------------------------------------------------------------------------
// appData: a micro-SQL over in-page tables (enough for the docs examples).

const appDataTables = new Map<string, Record<string, Json>[]>();

function runAppData(op: string, sql: string, params: Json[]): Json {
  const bind = (raw: string): Json => {
    const m = raw.trim().match(/^\$(\d+)$/);
    if (m) return params[Number(m[1]) - 1] ?? null;
    const s = raw.trim();
    if (/^'.*'$/.test(s)) return s.slice(1, -1);
    if (/^-?[0-9.]+$/.test(s)) return Number(s);
    if (s === 'null') return null;
    return s;
  };
  let m = sql.match(/^\s*insert\s+into\s+(\w+)\s*\(([^)]*)\)\s*values\s*\(([^)]*)\)\s*;?\s*$/i);
  if (m) {
    const cols = m[2].split(',').map((c) => c.trim());
    const vals = m[3].split(',').map(bind);
    const row = Object.fromEntries(cols.map((c, i) => [c, vals[i] ?? null]));
    const table = appDataTables.get(m[1].toLowerCase()) ?? [];
    table.push(row);
    appDataTables.set(m[1].toLowerCase(), table);
    return { rowCount: 1 };
  }
  m = sql.match(/^\s*select\s+\*\s+from\s+(\w+)\s*(?:where\b.*|order\s+by\b.*|limit\b.*)?;?\s*$/i);
  if (m) return { rows: (appDataTables.get(m[1].toLowerCase()) ?? []) as unknown as Json };
  m = sql.match(/^\s*delete\s+from\s+(\w+)\s*;?\s*$/i);
  if (m) {
    const n = (appDataTables.get(m[1].toLowerCase()) ?? []).length;
    appDataTables.set(m[1].toLowerCase(), []);
    return { rowCount: n };
  }
  if (/^\s*create\s+table/i.test(sql)) return { rowCount: 0 };
  return { appDataError: { kind: 'unsupported_statement', note: 'the docs VM speaks a micro-SQL: INSERT INTO t (c) VALUES ($1), SELECT * FROM t, DELETE FROM t', sql } };
}

// ---------------------------------------------------------------------------
// memory: persists across runs (and reloads) in localStorage.

function readMemory(): Record<string, Record<string, Json>> {
  try {
    return JSON.parse(localStorage.getItem(MEMORY_STORAGE) ?? '{}');
  } catch {
    return {};
  }
}

function writeMemory(all: Record<string, Record<string, Json>>): void {
  try {
    localStorage.setItem(MEMORY_STORAGE, JSON.stringify(all));
  } catch {
    /* storage blocked — memory just won't persist */
  }
}

function runMemoryOp(namespace: string, payload: Record<string, Json>): Json {
  const all = readMemory();
  const ns = all[namespace] ?? {};
  switch (payload.action) {
    case 'set':
      ns[String(payload.key)] = payload.value ?? null;
      all[namespace] = ns;
      writeMemory(all);
      return null;
    case 'get':
      return ns[String(payload.key)] ?? null;
    case 'list': {
      const prefix = payload.prefix === null || payload.prefix === undefined ? '' : String(payload.prefix);
      return Object.keys(ns).filter((k) => k.startsWith(prefix)).sort();
    }
    case 'delete': {
      const existed = String(payload.key) in ns;
      delete ns[String(payload.key)];
      all[namespace] = ns;
      writeMemory(all);
      return existed;
    }
    case 'clear':
      all[namespace] = {};
      writeMemory(all);
      return null;
    default:
      throw new Error(`memory: unknown action ${String(payload.action)}`);
  }
}

// ---------------------------------------------------------------------------

export interface DocsRunHost {
  /** Host options for BrowserAgent.start / .restore. */
  hostOptions: Record<string, unknown>;
  /** Deliver a signal to the main run (server routes, terminal command). */
  deliver: (msg: SignalMessage) => void;
  /** Cancel everything (panel Clear / terminal Ctrl-C). */
  cancel: () => void;
  /** Live actor entries, for status displays. */
  actors: () => { pid: string; scope: string; name: string | null; status: string }[];
  /** Recorded branch outcomes, for the CLI's branch store. */
  branchOutcomes: () => { label: string; branchId: string; status: string }[];
}

export interface RunHostOptions {
  vfs: Vfs;
  /** The agent file's directory — anchors workspace, templates, sub-agents. */
  projectDir: string;
  ui: RunUi;
  engine: EngineAssets;
  /** Skip ask-posture approval gates (CLI --trusted; the panel always). */
  trusted?: boolean;
  memoryNamespace?: string;
  getDocsTools?: () => Record<string, (kwargs: Json) => Json | Promise<Json>>;
}

/**
 * Build the full host for one docs run. `who` labels nested runs
 * ("actor researcher-1", "branch outline-first") in UI surfaces.
 */
export function createRunHost(options: RunHostOptions): DocsRunHost {
  const { vfs, projectDir, ui, engine } = options;
  const cancelled = { current: false };
  const hang = () => new Promise<never>(() => {});

  const mainMailbox = newMailbox();
  const actorTable = new Map<string, ActorEntry>();
  const namesToPids = new Map<string, string>();
  const branchLog: { label: string; branchId: string; status: string }[] = [];
  let pidCounter = 0;
  let branchCounter = 0;

  const approvedAll = new Set<string>();
  const gate = async (what: string, target: string): Promise<void> => {
    if (options.trusted || !ui.approve) return;
    const key = `${what}:${target}`;
    if (approvedAll.has(key)) return;
    const verdict = await ui.approve(what, target);
    if (verdict === 'all') approvedAll.add(key);
    else if (verdict !== 'yes') throw new Error(`${what} ${target}: denied by approval gate`);
  };

  const resolveSource = (source: string): { path: string; code: string } | null => {
    const abs = resolvePath(projectDir, source);
    const code = vfs.read(abs);
    return code === null ? null : { path: abs, code };
  };

  // ---- signal waits (main run and actors share this machinery) ----------

  const waitOnMailbox = (
    box: Mailbox,
    names: string[],
    timeoutMs: number | null,
    mode: 'signal' | 'receive',
    who: string,
    interactive: boolean,
  ): Promise<SignalMessage> => {
    const queued = takeQueued(box, names);
    if (queued) return Promise.resolve(queued);
    return new Promise<SignalMessage>((resolve) => {
      const waiter = {
        names,
        resolve: (msg: SignalMessage) => {
          done();
          resolve(msg);
        },
      };
      box.waiters.push(waiter);
      const timeoutMsg: SignalMessage = { name: null, payload: null, from: null, timedOut: true };
      let req: SignalRequest | null = null;
      const done = () => {
        if (req) ui.signalDone(req);
        box.waiters = box.waiters.filter((w) => w !== waiter);
      };
      const fire = (msg: SignalMessage) => {
        if (!box.waiters.includes(waiter)) return;
        done();
        resolve(msg);
      };
      // Short timeouts run on a real clock; long ones (a 24h review window)
      // wait for the reader's "let it time out" — the docs VM can't sit
      // through a day, and auto-firing would spin maintenance loops forever.
      if (timeoutMs !== null && timeoutMs > 0 && timeoutMs <= 30_000) {
        setTimeout(() => fire(timeoutMsg), timeoutMs);
      }
      if (interactive) {
        req = {
          names,
          timeoutMs,
          mode,
          who,
          deliver: (msg) => fire(msg),
          fireTimeout: () => fire(timeoutMsg),
        };
        ui.waitSignal(req);
      }
    });
  };

  const handleSignalEffect = (
    box: Mailbox,
    who: string,
    interactive: boolean,
  ) => async (payload: { name: string[] | string; opts?: Json }): Promise<Json> => {
    if (cancelled.current) return hang();
    const opts = asObj(payload.opts ?? null);
    const names = (Array.isArray(payload.name) ? payload.name : [payload.name]).map(String);
    const mode = String(opts.mode ?? 'signal');
    if (mode === 'poll') return (takeQueued(box, names) as unknown as Json) ?? null;
    if (mode === 'alarm') {
      const ms = Number(opts.ms ?? 0);
      const wait = Math.min(Math.max(ms, 0), 2000);
      ui.note?.(ms > wait ? `⏰ alarm(${ms} ms) fast-forwarded by the docs VM (${wait} ms real time)` : `⏰ alarm(${ms} ms)`);
      await new Promise((r) => setTimeout(r, wait));
      return { name: null, payload: null, from: null, timedOut: true } as unknown as Json;
    }
    const timeoutMs = opts.timeoutMs === null || opts.timeoutMs === undefined ? null : Number(opts.timeoutMs);
    ui.refresh?.();
    const msg = await waitOnMailbox(box, names, timeoutMs, mode === 'receive' ? 'receive' : 'signal', who, interactive);
    if (cancelled.current) return hang();
    return msg as unknown as Json;
  };

  // ---- nested runs: actors, agents, branches, sub-agents -----------------

  interface NestedOutcome {
    status: 'completed' | 'paused' | 'failed';
    output: Json;
    pendingPrompt?: string;
    error?: string;
  }

  const runNested = async (
    code: string,
    sourceDir: string,
    input: Json,
    who: string,
    opts: {
      mailbox?: Mailbox;
      stopFlag?: { stopped: boolean };
      onWaiting?: (names: string[] | null) => void;
      /** How chidori.input inside the nested run behaves. */
      inputMode: 'suspend' | 'auto' | 'forward';
    },
  ): Promise<NestedOutcome> => {
    const nestedBox = opts.mailbox ?? newMailbox();
    const baseSignal = handleSignalEffect(nestedBox, who, false);
    try {
      const agent = engine.sdk.BrowserAgent.start(engine.wasm, {
        source: buildHarnessSource(code, input),
        llm: async (payload: Json) => {
          if (cancelled.current || opts.stopFlag?.stopped) return hang();
          ui.busy?.(`${who}: calling the model…`);
          ui.refresh?.();
          return decidePrompt(payload as { text: string; opts?: unknown });
        },
        tools: makeToolTable(who, nestedBox),
        fetchImpl: fetchWithSimulatedFallback as typeof fetch,
        onSignal: async (payload: { name: string[] | string; opts?: Json }) => {
          if (cancelled.current || opts.stopFlag?.stopped) return hang();
          const names = (Array.isArray(payload.name) ? payload.name : [payload.name]).map(String);
          opts.onWaiting?.(names);
          try {
            return await baseSignal(payload);
          } finally {
            opts.onWaiting?.(null);
          }
        },
        onInput:
          opts.inputMode === 'suspend'
            ? undefined
            : async (payload: { prompt: string; opts?: Json }) => {
                if (cancelled.current || opts.stopFlag?.stopped) return hang();
                if (opts.inputMode === 'forward') return ui.askInput(payload, who);
                const def = asObj(payload.opts ?? null).default;
                const answer = def === undefined || def === null ? 'yes' : String(def);
                ui.note?.(`${who}: input("${payload.prompt}") auto-answered "${answer}" (nested runs don't prompt)`);
                return answer;
              },
      });
      const result = await agent.run();
      if (result.status === 'suspended') {
        return { status: 'paused', output: null, pendingPrompt: result.pendingInput?.prompt ?? '' };
      }
      // The harness journals the handler's return as the final result event.
      let output: Json = null;
      for (const line of result.console) {
        try {
          const ev = JSON.parse(line);
          if (ev && ev.k === 'result') output = ev.value ?? null;
        } catch {
          /* plain console text */
        }
      }
      return { status: 'completed', output };
    } catch (err) {
      return { status: 'failed', output: null, error: String(err instanceof Error ? err.message : err) };
    }
  };

  const spawn = (scope: 'actors' | 'agents', payload: Record<string, Json>): Json => {
    const source = String(payload.source ?? '');
    const input = payload.input ?? {};
    const spawnOpts = asObj(payload.options ?? null);
    const pid = `${scope === 'agents' ? 'agent' : 'actor'}-${++pidCounter}`;
    const name = spawnOpts.name === undefined || spawnOpts.name === null ? null : String(spawnOpts.name);
    const resolved = resolveSource(source);
    const stopFlag = { stopped: false };
    const entry: ActorEntry = {
      pid,
      scope,
      name,
      source,
      status: 'running',
      restarts: 0,
      simulated: !resolved,
      mailbox: newMailbox(),
      waitingFor: null,
      output: null,
      error: null,
      done: Promise.resolve(),
      stopFlag,
    };
    actorTable.set(pid, entry);
    if (name) namesToPids.set(name, pid);
    if (!resolved) {
      // No such source in the docs VM — a clearly labelled simulated actor
      // that settles immediately, so joins don't hang.
      entry.status = 'completed';
      entry.output = {
        __simulated: true,
        note: `the docs VM has no file at "${source}" — this ${scope === 'agents' ? 'detached agent' : 'actor'} is simulated`,
      };
      ui.note?.(`${scope}.spawn(${source}): source not in the docs VM filesystem — simulated`);
      return { pid, name, runId: pid, simulated: true };
    }
    entry.done = runNested(resolved.code, dirname(resolved.path), input, `${scope === 'agents' ? 'agent' : 'actor'} ${name ?? pid}`, {
      mailbox: entry.mailbox,
      stopFlag,
      onWaiting: (names) => {
        entry.waitingFor = names;
        entry.status = names && scope === 'agents' ? 'hibernating' : entry.status === 'running' || entry.status === 'hibernating' ? 'running' : entry.status;
        if (names && scope === 'agents') entry.status = 'hibernating';
        ui.refresh?.();
      },
      inputMode: 'auto',
    }).then((outcome) => {
      if (entry.status === 'stopped') return;
      entry.status = outcome.status === 'completed' ? 'completed' : 'failed';
      entry.output = outcome.output;
      entry.error = outcome.error ?? null;
      if (outcome.status === 'failed') {
        deliverTo(mainMailbox, {
          name: '__chidori.down__',
          payload: { pid, status: 'failed', error: outcome.error ?? 'unknown' },
          from: { kind: 'agent', id: pid },
        });
      }
      ui.refresh?.();
    });
    return { pid, name, runId: pid, simulated: false };
  };

  const findActor = (target: string): ActorEntry | null =>
    actorTable.get(target) ?? actorTable.get(namesToPids.get(target) ?? '') ?? null;

  const actorSnapshot = (entry: ActorEntry): Json => ({
    pid: entry.pid,
    name: entry.name,
    runId: entry.pid,
    status: entry.status,
    restarts: entry.restarts,
    mailbox: entry.mailbox.queue.length,
    ...(entry.waitingFor ? { waitingFor: entry.waitingFor } : {}),
  });

  const settled = (entry: ActorEntry) => entry.status !== 'running' && entry.status !== 'hibernating';

  const runActorsOp = async (payload: Record<string, Json>): Promise<Json> => {
    const scope = payload.scope === 'agents' ? 'agents' : 'actors';
    const op = String(payload.op ?? '');
    if (op === 'spawn') return spawn(scope, payload);
    const target = String(payload.target ?? '');
    if (op === 'lookup') {
      const entry = findActor(target);
      return entry ? { pid: entry.pid, name: entry.name, runId: entry.pid, simulated: entry.simulated } : null;
    }
    const entry = findActor(target);
    if (op === 'send') {
      // Actors address their spawner as "parent".
      if (!entry && target === 'parent') {
        deliverTo(mainMailbox, { name: String(payload.message), payload: payload.payload ?? null, from: { kind: 'agent', id: 'child' } });
        ui.refresh?.();
        return { delivered: true };
      }
      if (!entry || settled(entry)) return { delivered: false };
      deliverTo(entry.mailbox, { name: String(payload.message), payload: payload.payload ?? null, from: { kind: 'agent', id: 'parent' } });
      ui.refresh?.();
      return { delivered: true };
    }
    if (!entry) throw new Error(`${scope}: no such ${scope === 'agents' ? 'agent' : 'actor'}: ${target}`);
    switch (op) {
      case 'status':
        return actorSnapshot(entry);
      case 'join': {
        const timeoutMs = payload.timeoutMs === null || payload.timeoutMs === undefined ? null : Number(payload.timeoutMs);
        if (!settled(entry)) {
          const finished = await Promise.race([
            entry.done.then(() => true),
            timeoutMs === null ? hang() : new Promise<false>((r) => setTimeout(() => r(false), Math.min(timeoutMs, 60_000))),
          ]);
          if (!finished) return actorSnapshot(entry);
        }
        return {
          ...(actorSnapshot(entry) as Record<string, Json>),
          ...(entry.output !== null ? { output: entry.output } : {}),
          ...(entry.error !== null ? { error: entry.error } : {}),
        };
      }
      case 'stop': {
        if (!settled(entry)) {
          entry.stopFlag.stopped = true;
          entry.status = 'stopped';
          // Wake any parked signal wait so the nested run doesn't leak.
          for (const waiter of entry.mailbox.waiters.splice(0)) {
            waiter.resolve({ name: null, payload: null, from: null, timedOut: true });
          }
        }
        return actorSnapshot(entry);
      }
      default:
        throw new Error(`${scope}: unknown op ${op}`);
    }
  };

  const runBranch = async (payload: Record<string, Json>): Promise<Json> => {
    const variants = Array.isArray(payload.variants) ? payload.variants : [];
    const concurrency = Math.max(1, Number(asObj(payload.options ?? null).concurrency ?? 1));
    const seq = ++branchCounter;
    const outcomes: Json[] = new Array(variants.length);
    let next = 0;
    const lane = async () => {
      for (;;) {
        const k = next++;
        if (k >= variants.length) return;
        const variant = asObj(variants[k]);
        const label = String(variant.label ?? `branch-${k}`);
        const branchId = `op${seq}-branch-${k}`;
        const resolved = resolveSource(String(variant.source ?? ''));
        if (!resolved) {
          outcomes[k] = { label, branchId, status: 'failed', error: `no such source in the docs VM: ${String(variant.source)}` };
        } else {
          const outcome = await runNested(resolved.code, dirname(resolved.path), variant.input ?? {}, `branch ${label}`, {
            inputMode: 'suspend',
          });
          outcomes[k] = {
            label,
            branchId,
            status: outcome.status,
            ...(outcome.output !== null ? { output: outcome.output } : {}),
            ...(outcome.pendingPrompt !== undefined ? { pendingPrompt: outcome.pendingPrompt } : {}),
            ...(outcome.error !== undefined ? { error: outcome.error } : {}),
          };
        }
        branchLog.push({ label, branchId, status: String(asObj(outcomes[k]).status) });
        ui.refresh?.();
      }
    };
    await Promise.all(Array.from({ length: Math.min(concurrency, variants.length) }, lane));
    return outcomes;
  };

  const runSubagent = async (payload: Record<string, Json>): Promise<Json> => {
    const path = String(payload.path ?? '');
    const resolved = resolveSource(path);
    if (!resolved) {
      return { __simulated: true, note: `the docs VM has no file at "${path}" — simulated sub-agent result` };
    }
    const outcome = await runNested(resolved.code, dirname(resolved.path), payload.input ?? {}, `sub-agent ${path}`, {
      inputMode: 'forward',
    });
    if (outcome.status === 'failed') throw new Error(`callAgent(${path}): ${outcome.error}`);
    if (outcome.status === 'paused') throw new Error(`callAgent(${path}): sub-agent paused at input("${outcome.pendingPrompt}")`);
    return outcome.output;
  };

  const runWorkspace = async (payload: Record<string, Json>): Promise<Json> => {
    const action = String(payload.action ?? '');
    const rel = String(payload.path ?? '');
    const abs = resolvePath(projectDir, rel);
    const entryFor = (relPath: string, content: string): Json => ({
      path: relPath,
      status: 'complete',
      sha256: contentHash(content),
      bytes: content.length,
    });
    switch (action) {
      case 'read': {
        const content = vfs.read(abs);
        return content === null ? { __missing: true } : content;
      }
      case 'write': {
        await gate('workspace:write', rel);
        vfs.write(abs, String(payload.content ?? ''));
        ui.refresh?.();
        return entryFor(rel, String(payload.content ?? ''));
      }
      case 'delete': {
        await gate('workspace:delete', rel);
        vfs.delete(abs);
        ui.refresh?.();
        return null;
      }
      case 'list':
      case 'manifest': {
        const entries = vfs
          .walk(projectDir)
          .filter((p) => !p.includes('/.chidori/'))
          .map((p) => entryFor(p.slice(projectDir.length + 1), vfs.read(p) ?? ''));
        return action === 'list' ? entries : { root: projectDir, entries };
      }
      default:
        throw new Error(`workspace: unknown action ${action}`);
    }
  };

  // ---- the tool table (docs tools + the harness's internal ops) ---------

  const makeToolTable = (who: string, mailboxForActors: Mailbox): Record<string, (kwargs: Json) => Json | Promise<Json>> => {
    const base = options.getDocsTools ? options.getDocsTools() : makeDocsTools(() => null);
    const wrapped: Record<string, (kwargs: Json) => Json | Promise<Json>> = {};
    for (const [name, impl] of Object.entries(base)) {
      wrapped[name] = async (kwargs) => {
        if (cancelled.current) return hang();
        await gate('tool', name);
        ui.busy?.(`${who}: running ${name}…`);
        ui.refresh?.();
        return impl(kwargs);
      };
    }
    wrapped[INTERNAL_TOOLS.workspace] = (kwargs) => runWorkspace(asObj(kwargs));
    wrapped[INTERNAL_TOOLS.memory] = (kwargs) => runMemoryOp(options.memoryNamespace ?? 'default', asObj(kwargs));
    wrapped[INTERNAL_TOOLS.appData] = (kwargs) => {
      const p = asObj(kwargs);
      return runAppData(String(p.op), String(p.sql ?? ''), Array.isArray(p.params) ? p.params : []);
    };
    wrapped[INTERNAL_TOOLS.actors] = (kwargs) => {
      // "parent" from inside an actor means that actor's spawner: the main
      // run's mailbox in this flat two-level docs VM.
      void mailboxForActors;
      return runActorsOp(asObj(kwargs));
    };
    wrapped[INTERNAL_TOOLS.branch] = (kwargs) => runBranch(asObj(kwargs));
    wrapped[INTERNAL_TOOLS.subagent] = (kwargs) => runSubagent(asObj(kwargs));
    return wrapped;
  };

  const hostOptions: Record<string, unknown> = {
    llm: async (payload: Json) => {
      if (cancelled.current) return hang();
      ui.busy?.('calling the model…');
      ui.refresh?.();
      return decidePrompt(payload as { text: string; opts?: unknown });
    },
    tools: makeToolTable('run', mainMailbox),
    fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
      await gate('http', String(input));
      return fetchWithSimulatedFallback(input, init);
    }) as typeof fetch,
    onSignal: handleSignalEffect(mainMailbox, 'run', true),
    onInput: async (payload: { prompt: string; opts?: Json }) => {
      if (cancelled.current) return hang();
      ui.busy?.(null);
      ui.refresh?.();
      return ui.askInput(payload, 'run');
    },
  };

  return {
    hostOptions,
    deliver: (msg) => {
      deliverTo(mainMailbox, msg);
      ui.refresh?.();
    },
    cancel: () => {
      cancelled.current = true;
      for (const entry of actorTable.values()) entry.stopFlag.stopped = true;
    },
    actors: () => [...actorTable.values()].map((e) => ({ pid: e.pid, scope: e.scope, name: e.name, status: e.status })),
    branchOutcomes: () => branchLog.slice(),
  };
}
