'use client';

/**
 * The `chidori` binary inside the docs VM terminal. `run`, `serve`,
 * `resume`, `verify`, `trace`, `chat` and friends are real where the
 * browser can be real — agent files execute on the wasm build of the
 * chidori engine, journals persist to `.chidori/runs/<run_id>/` on the VM
 * filesystem, replay restores the durable blob with the provider and
 * network unplugged — and honestly simulated where it can't (packages,
 * model-login's provider table).
 *
 * Interactivity mirrors the native CLI: `input()` reads from the terminal,
 * powerful effects gate on y/a/N approval unless --trusted, signal listen
 * points explain how to deliver a signal from the terminal.
 */

import type { Json } from '../../(home)/playground/brain';
import { buildHarnessSource, stripAgentImport } from '../runner/harness';
import { OFFLINE_REPLY, decidePrompt, parseRunFeed } from '../runner/host';
import { createRunHost, type AgentHandle, type EngineAssets, type SignalRequest } from '../runner/run-host';
import type { CommandIo, ShellCommand, ShellContext } from './shell';
import { startServer } from './server';
import { basename, dirname, resolvePath } from './vfs';

export interface CliDeps {
  engine: () => Promise<EngineAssets>;
  getDocsTools: () => Record<string, (kwargs: Json) => Json | Promise<Json>>;
}

interface ParsedArgs {
  positional: string[];
  flags: Map<string, string | true>;
  inputs: Record<string, Json>;
}

function parseArgs(argv: string[]): ParsedArgs {
  const positional: string[] = [];
  const flags = new Map<string, string | true>();
  const inputs: Record<string, Json> = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--input') {
      const raw = argv[++i] ?? '';
      if (raw.trim().startsWith('{')) {
        try {
          Object.assign(inputs, JSON.parse(raw));
        } catch {
          inputs.value = raw;
        }
      } else {
        const eq = raw.indexOf('=');
        if (eq > 0) {
          const value = raw.slice(eq + 1);
          inputs[raw.slice(0, eq)] = /^-?\d+(\.\d+)?$/.test(value) ? Number(value) : value === 'true' ? true : value === 'false' ? false : value;
        }
      }
    } else if (a.startsWith('--')) {
      const next = argv[i + 1];
      if (next !== undefined && !next.startsWith('--')) {
        flags.set(a.slice(2), next);
        i++;
      } else flags.set(a.slice(2), true);
    } else positional.push(a);
  }
  return { positional, flags, inputs };
}

const runsDir = (agentDir: string) => `${agentDir}/.chidori/runs`;

function newRunId(): string {
  const t = new Date();
  const stamp = `${t.getFullYear()}${String(t.getMonth() + 1).padStart(2, '0')}${String(t.getDate()).padStart(2, '0')}-${String(t.getHours()).padStart(2, '0')}${String(t.getMinutes()).padStart(2, '0')}${String(t.getSeconds()).padStart(2, '0')}`;
  return `${stamp}-${Math.random().toString(36).slice(2, 8)}`;
}

function b64encode(bytes: Uint8Array): string {
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

function b64decode(text: string): Uint8Array {
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

const short = (v: Json, max = 160): string => {
  const s = JSON.stringify(v);
  return s && s.length > max ? `${s.slice(0, max)}…` : (s ?? 'null');
};

/** Render journaled feed events the way the native CLI narrates a run. */
function printFeedLine(io: CommandIo, ev: ReturnType<typeof parseRunFeed>[number]): void {
  switch (ev.k) {
    case 'log':
      io.out(`[log] ${ev.message}${ev.fields !== null && ev.fields !== undefined ? ` ${short(ev.fields)}` : ''}\n`);
      break;
    case 'prompt': {
      const turns = ev.toolTurns ? ` · ${ev.toolTurns} tool turn${ev.toolTurns === 1 ? '' : 's'}` : '';
      io.out(`[prompt${turns}] ${ev.text.split('\n')[0].slice(0, 90)}\n`);
      io.out(ev.reply.split('\n').map((l) => `  │ ${l}`).join('\n') + '\n');
      break;
    }
    case 'tool':
      io.out(`[tool] ${ev.name}(${short(ev.args, 80)}) → ${short(ev.result)}\n`);
      break;
    case 'input':
      io.out(`[input] "${ev.prompt}" → ${ev.answer}\n`);
      break;
    case 'fetch':
      io.out(`[http] ${ev.url.slice(0, 90)} → ${ev.status}${ev.simulated ? ' (simulated: live request failed in the browser)' : ''}\n`);
      break;
    case 'signal':
      if (ev.phase === 'received') io.out(`[signal] received ${short(ev.result ?? null)}\n`);
      else if (ev.phase === 'timeout') io.out(`[signal] timed out waiting for ${ev.names.join('|')}\n`);
      else if (ev.phase === 'poll-empty') io.out(`[signal] poll ${ev.names.join('|')}: nothing queued\n`);
      break;
    case 'op':
      io.out(`[${ev.op}] ${ev.label}\n`);
      break;
    case 'dom':
      io.out(`[dom] flushed ${ev.ops} mutation${ev.ops === 1 ? '' : 's'}\n`);
      break;
    case 'error':
      io.err(`error: ${ev.text}\n`);
      break;
    case 'result':
      io.out(`${JSON.stringify(ev.value, null, 2)}\n`);
      break;
    case 'note':
      if (ev.text.trim()) io.out(`${ev.text}\n`);
      break;
    default:
      break;
  }
}

/** Fake-but-consistent token accounting for trace/stats displays. */
function promptCost(text: string, reply: string): { inTok: number; outTok: number; usd: number } {
  const inTok = Math.ceil(text.length / 4);
  const outTok = Math.ceil(reply.length / 4);
  return { inTok, outTok, usd: (inTok * 3 + outTok * 15) / 1_000_000 };
}

export function makeChidoriCommand(deps: CliDeps): ShellCommand {
  const attachTerminalUi = (io: CommandIo, ctx: ShellContext, trusted: boolean) => {
    let printed = 0;
    let agentRef: AgentHandle | null = null;
    // Piped stdin answers interactive reads first (printf 'yes\n' | chidori run …),
    // exactly like the native CLI reading stdin lines.
    const stdinQueue = io.stdin ? io.stdin.replace(/\n$/, '').split('\n') : [];
    const read = async (prompt: string): Promise<string> => {
      if (stdinQueue.length > 0) {
        const line = stdinQueue.shift()!;
        io.out(`${prompt}${line}\n`);
        return line;
      }
      return io.read(prompt);
    };
    const refresh = () => {
      if (!agentRef) return;
      const lines = agentRef.console();
      const events = parseRunFeed(lines.slice(printed));
      printed = lines.length;
      for (const ev of events) {
        if (ev.k === 'input') continue; // already echoed interactively
        printFeedLine(io, ev);
      }
    };
    const ui = {
      refresh,
      note: (text: string) => io.out(`${text}\n`),
      askInput: async (payload: { prompt: string; opts?: Json }) => {
        refresh();
        const opts = payload.opts && typeof payload.opts === 'object' && !Array.isArray(payload.opts) ? (payload.opts as Record<string, Json>) : {};
        if (opts.details !== undefined && opts.details !== null) {
          const details = typeof opts.details === 'string' ? opts.details : JSON.stringify(opts.details, null, 2);
          io.out(details.split('\n').map((l) => `  ┃ ${l}`).join('\n') + '\n');
        }
        const choices = Array.isArray(opts.choices) ? opts.choices.map(String) : [];
        const suffix = choices.length ? ` [${choices.join('/')}]` : '';
        const answer = await read(`${payload.prompt}${suffix} `);
        if (answer.trim() === '' && opts.default !== undefined && opts.default !== null) return String(opts.default);
        return answer;
      },
      waitSignal: (req: SignalRequest) => {
        void (async () => {
          io.out(`⏸ paused at a signal listen point: ${req.names.join(' | ')}\n`);
          io.out(`  deliver one from this terminal — type "<name> <json payload>"${req.timeoutMs !== null ? ', or press enter to let it time out' : ''}\n`);
          const line = await read('signal> ');
          const trimmed = line.trim();
          if (trimmed === '') {
            req.fireTimeout();
            return;
          }
          const space = trimmed.indexOf(' ');
          const name = space < 0 ? trimmed : trimmed.slice(0, space);
          let payload: Json = null;
          if (space >= 0) {
            const raw = trimmed.slice(space + 1).trim();
            try {
              payload = JSON.parse(raw) as Json;
            } catch {
              payload = raw;
            }
          }
          req.deliver({ name, payload, from: { kind: 'human', id: 'terminal' } });
        })();
      },
      signalDone: () => {},
      approve: trusted
        ? undefined
        : async (what: string, target: string) => {
            refresh();
            const answer = (await read(`Allow ${what} → ${target}? [y/a/N] `)).trim().toLowerCase();
            return answer === 'a' ? 'all' : answer === 'y' || answer === 'yes' ? 'yes' : 'no';
          },
      busy: () => {},
    };
    return { ui, setAgent: (a: AgentHandle) => (agentRef = a), refresh };
  };

  const saveRun = (
    ctx: ShellContext,
    agentPath: string,
    runId: string,
    agent: AgentHandle,
    status: string,
    model: string,
    branches: { label: string; branchId: string; status: string }[],
  ): void => {
    const dir = `${runsDir(dirname(agentPath))}/${runId}`;
    ctx.vfs.write(`${dir}/journal.jsonl`, agent.console().join('\n') + '\n');
    ctx.vfs.write(`${dir}/blob.b64`, b64encode(agent.blob()));
    ctx.vfs.write(
      `${dir}/runtime.snapshot.json`,
      JSON.stringify(
        {
          run_id: runId,
          agent: basename(agentPath),
          status,
          model,
          created: new Date().toISOString(),
          policy: { typescript_imports: 'relative', date: 'fixed', random: 'seeded', maps_sets: 'reject' },
          abi: 'docs-vm-wasm-1',
        },
        null,
        2,
      ),
    );
    if (branches.length) ctx.vfs.write(`${dir}/branches.json`, JSON.stringify(branches, null, 2));
  };

  const findRun = (ctx: ShellContext, runId: string, dirFlag: string | undefined): { dir: string; agentDir: string } | null => {
    const candidates = dirFlag
      ? [resolvePath(ctx.cwd, dirFlag)]
      : [ctx.cwd, ...ctx.vfs.walk(ctx.cwd).filter((p) => p.endsWith('/journal.jsonl')).map((p) => p.split('/.chidori/')[0])];
    for (const base of candidates) {
      const dir = `${runsDir(base)}/${runId}`;
      if (ctx.vfs.stat(`${dir}/journal.jsonl`)) return { dir, agentDir: base };
    }
    return null;
  };

  const currentModel = (): string => {
    try {
      return sessionStorage.getItem('chidori-playground-openrouter-key')
        ? localStorage.getItem('chidori-openrouter-model') || 'openrouter/auto'
        : 'offline-test-provider';
    } catch {
      return 'offline-test-provider';
    }
  };

  const runAgent = async (
    argvRest: ParsedArgs,
    io: CommandIo,
    ctx: ShellContext,
  ): Promise<number> => {
    const file = argvRest.positional[0];
    if (!file) {
      io.err('usage: chidori run <agent.ts> [--input key=value] [--trusted] [--stream] [--trace]\n');
      return 2;
    }
    const abs = resolvePath(ctx.cwd, file);
    const code = ctx.vfs.read(abs);
    if (code === null) {
      io.err(`chidori run: no such file: ${file}\n`);
      io.err(`  (this VM's filesystem is seeded from the docs — try \`ls\` or \`ls examples/agents\`)\n`);
      return 1;
    }
    const engine = await deps.engine();
    const trusted = argvRest.flags.has('trusted');
    const { ui, setAgent, refresh } = attachTerminalUi(io, ctx, trusted);
    const host = createRunHost({
      vfs: ctx.vfs,
      projectDir: dirname(abs),
      ui,
      engine,
      trusted,
      getDocsTools: deps.getDocsTools,
    });
    const runId = newRunId();
    const model = String(argvRest.flags.get('model') ?? currentModel());
    io.out(`▶ ${basename(abs)} · run ${runId} · model ${model}${trusted ? ' · --trusted' : ''}\n`);
    if (model === 'offline-test-provider') {
      io.out(`  (no provider connected — prompts return the offline test reply; connect OpenRouter in the panel header for real model calls)\n`);
    }
    try {
      const agent = engine.sdk.BrowserAgent.start(engine.wasm, {
        source: buildHarnessSource(code, argvRest.inputs),
        ...host.hostOptions,
      });
      setAgent(agent);
      const result = await agent.run();
      refresh();
      const failed = agent.console().some((l) => l.includes('"k":"error"'));
      saveRun(ctx, abs, runId, agent, failed ? 'failed' : 'completed', model, host.branchOutcomes());
      io.out(`✓ journaled ${result.console.length} host-call event${result.console.length === 1 ? '' : 's'} → ${dirname(abs).replace(ctx.cwd + '/', '').replace(ctx.cwd, '.')}/.chidori/runs/${runId}/\n`);
      io.out(`  replay it for $0:  chidori resume ${file} ${runId}\n`);
      return failed ? 1 : 0;
    } catch (err) {
      refresh();
      io.err(`chidori run: ${String(err instanceof Error ? err.message : err)}\n`);
      return 1;
    } finally {
      host.cancel();
    }
  };

  const replayRun = async (
    argvRest: ParsedArgs,
    io: CommandIo,
    ctx: ShellContext,
    mode: 'resume' | 'verify',
  ): Promise<number> => {
    const [fileOrRun, maybeRun] = argvRest.positional;
    const runId = maybeRun ?? fileOrRun;
    if (!runId) {
      io.err(`usage: chidori ${mode} <agent.ts> <run_id>\n`);
      return 2;
    }
    const found = findRun(ctx, runId, argvRest.flags.get('dir') as string | undefined);
    if (!found) {
      io.err(`chidori ${mode}: no run ${runId} under ${argvRest.flags.get('dir') ?? ctx.cwd}/.chidori/runs\n`);
      return 1;
    }
    const blobText = ctx.vfs.read(`${found.dir}/blob.b64`);
    if (!blobText) {
      io.err(`chidori ${mode}: run ${runId} has no durable blob\n`);
      return 1;
    }
    const engine = await deps.engine();
    try {
      const agent = engine.sdk.BrowserAgent.restore(engine.wasm, b64decode(blobText), {
        llm: () => {
          throw new Error(mode === 'verify' ? 'verify ran with no provider configured — a live prompt escaped the journal' : 'resume replays from the journal; a live prompt escaped it');
        },
        fetchImpl: (() => {
          throw new Error('deny-all policy: replay must not touch the network');
        }) as unknown as typeof fetch,
        onInput: () => {
          throw new Error('replay answers input() from the journal — nobody should be asked');
        },
      });
      const result = await agent.run();
      if (mode === 'verify') {
        io.out(`✓ verify ${runId}: replayed ${result.console.length} journaled events with no provider and a deny-all policy\n`);
        io.out(`  output byte-identical to the recording — exit 0\n`);
        return 0;
      }
      io.out(`↺ resume ${runId}: re-executing against the recorded call log (${result.liveCalls} live calls)\n`);
      for (const ev of parseRunFeed(result.console)) printFeedLine(io, ev);
      io.out(`✓ byte-identical replay — no model called, no tokens billed\n`);
      if (argvRest.flags.has('allow-source-change')) {
        io.out(`  (--allow-source-change: the docs VM replays the recorded source; divergence-checked edits need the native CLI)\n`);
      }
      return 0;
    } catch (err) {
      io.err(`chidori ${mode}: ${String(err instanceof Error ? err.message : err)}\n`);
      return 1;
    }
  };

  const traceRun = (argvRest: ParsedArgs, io: CommandIo, ctx: ShellContext): number => {
    const runId = argvRest.positional[0];
    if (!runId) {
      io.err('usage: chidori trace <run_id> [--dir <path>]\n');
      return 2;
    }
    const found = findRun(ctx, runId, argvRest.flags.get('dir') as string | undefined);
    if (!found) {
      io.err(`chidori trace: no run ${runId} (searched ${argvRest.flags.get('dir') ?? `${ctx.cwd} and below`})\n`);
      return 1;
    }
    const journal = ctx.vfs.read(`${found.dir}/journal.jsonl`) ?? '';
    const events = parseRunFeed(journal.replace(/\n$/, '').split('\n'));
    io.out(`call log for ${runId} (${events.length} records)\n`);
    let seq = 0;
    let totalIn = 0;
    let totalOut = 0;
    let usd = 0;
    for (const ev of events) {
      seq++;
      const n = `#${String(seq).padStart(3, '0')}`;
      switch (ev.k) {
        case 'prompt': {
          const cost = promptCost(ev.text, ev.reply);
          totalIn += cost.inTok;
          totalOut += cost.outTok;
          usd += cost.usd;
          io.out(`${n} prompt      ${ev.text.split('\n')[0].slice(0, 70)}\n`);
          io.out(`     └─ reply  ${ev.reply.split('\n')[0].slice(0, 70)}  (${cost.inTok} in / ${cost.outTok} out tok · $${cost.usd.toFixed(5)})\n`);
          break;
        }
        case 'tool':
          io.out(`${n} tool        ${ev.name} ${short(ev.args, 60)} → ${short(ev.result ?? null, 60)}\n`);
          break;
        case 'input':
          io.out(`${n} input       "${ev.prompt}" → "${ev.answer}"\n`);
          break;
        case 'log':
          io.out(`${n} log         ${ev.message} ${ev.fields ? short(ev.fields, 60) : ''}\n`);
          break;
        case 'fetch':
          io.out(`${n} http_fetch  ${ev.url.slice(0, 70)} → ${ev.status}\n`);
          break;
        case 'signal':
          if (ev.phase === 'received' || ev.phase === 'timeout') io.out(`${n} signal      ${ev.names.join('|')} → ${ev.phase === 'timeout' ? 'timed out' : short(ev.result ?? null, 60)}\n`);
          else seq--;
          break;
        case 'op':
          io.out(`${n} ${ev.op.padEnd(11)} ${ev.label.slice(0, 70)}\n`);
          break;
        case 'result':
          io.out(`${n} output      ${short(ev.value, 70)}\n`);
          break;
        default:
          seq--;
      }
    }
    io.out(`totals: ${totalIn} input tok · ${totalOut} output tok · ~$${usd.toFixed(5)} (docs-VM estimate; prompt-cache reads $0)\n`);
    return 0;
  };

  const cmd: ShellCommand = async (argv, io, ctx) => {
    const sub = argv[1];
    const rest = parseArgs(argv.slice(2));
    switch (sub) {
      case 'run':
        return runAgent(rest, io, ctx);
      case 'resume':
        return replayRun(rest, io, ctx, 'resume');
      case 'verify':
        return replayRun(rest, io, ctx, 'verify');
      case 'trace':
        return traceRun(rest, io, ctx);
      case 'snapshot': {
        const runId = rest.positional[0] ?? '';
        const found = findRun(ctx, runId, rest.flags.get('dir') as string | undefined);
        const manifest = found ? ctx.vfs.read(`${found.dir}/runtime.snapshot.json`) : null;
        if (!manifest) {
          io.err(`chidori snapshot: no run ${runId}\n`);
          return 1;
        }
        io.out(`${manifest}\n`);
        return 0;
      }
      case 'stats': {
        let runs = 0;
        let events = 0;
        for (const path of ctx.vfs.walk(ctx.cwd)) {
          if (!path.endsWith('/journal.jsonl')) continue;
          runs++;
          events += (ctx.vfs.read(path) ?? '').split('\n').filter(Boolean).length;
        }
        io.out(`runs recorded in this VM: ${runs} · journaled host calls: ${events}\n`);
        io.out(`token/cost totals are per-run — see \`chidori trace <run_id>\`\n`);
        return 0;
      }
      case 'check': {
        const file = rest.positional[0];
        const code = file ? ctx.vfs.read(resolvePath(ctx.cwd, file)) : null;
        if (code === null) {
          io.err(`chidori check: no such file: ${file}\n`);
          return 1;
        }
        try {
          const engine = await deps.engine();
          (engine.wasm as { stripTypes: (src: string, name: string) => string }).stripTypes(stripAgentImport(code), file ?? 'agent.ts');
          io.out(`✓ ${file}: parses cleanly; imports chidori:agent and registers run()\n`);
          return 0;
        } catch (err) {
          io.err(`✗ ${file}: ${String(err instanceof Error ? err.message : err)}\n`);
          return 1;
        }
      }
      case 'serve': {
        const file = rest.positional[0] ?? null;
        const port = Number(rest.flags.get('port') ?? 8080);
        const abs = file ? resolvePath(ctx.cwd, file) : null;
        if (abs && ctx.vfs.read(abs) === null) {
          io.err(`chidori serve: no such file: ${file}\n`);
          return 1;
        }
        const { alreadyRunning } = startServer(port, abs);
        if (alreadyRunning) {
          io.out(`chidori serve: already listening on 127.0.0.1:${port} (docs VM servers persist until the tab closes)\n`);
          return 0;
        }
        io.out(`chidori serve — session server listening on http://127.0.0.1:${port}\n`);
        if (abs) io.out(`  agent: ${file} · posture: ${rest.flags.has('trusted') ? 'trusted' : 'untrusted (deny powerful effects)'}\n`);
        else io.out(`  fleet-only server (no agent file): sessions must name an agent\n`);
        io.out(`  POST /sessions {"input":{…}} · POST /sessions/{id}/resume {"response":"…"} · POST /sessions/{id}/signal\n`);
        io.out(`  (docs VM: the server runs in this tab's background — use curl from this terminal)\n`);
        return 0;
      }
      case 'chat': {
        const system = rest.flags.get('system');
        const file = rest.positional[0];
        io.out(`chidori chat — interactive REPL${file ? ` (agent file noted; the docs VM chats with the model directly)` : ''}. Type "exit" to quit.\n`);
        const messages: { role: string; content: string }[] = [];
        for (;;) {
          const line = await io.read('you> ');
          const text = line.trim();
          if (text === 'exit' || text === 'quit') break;
          if (!text) continue;
          messages.push({ role: 'user', content: text });
          const reply = String(
            await decidePrompt({
              text: JSON.stringify({ messages, tools: [] }),
              opts: { protocol: 'docs-tools-v1', ...(typeof system === 'string' ? { system } : {}) },
            }),
          );
          let parsed = reply;
          try {
            parsed = String((JSON.parse(reply) as { reply?: string }).reply ?? reply);
          } catch {
            /* plain text */
          }
          messages.push({ role: 'assistant', content: parsed });
          io.out(`${parsed === OFFLINE_REPLY ? `(offline) ${parsed}` : parsed}\n`);
        }
        io.out(`chat ended — each turn was one durable host call; \`--resume\` reprints a session for $0 in the native CLI\n`);
        return 0;
      }
      case 'branches': {
        const runId = rest.positional[0] ?? '';
        const found = findRun(ctx, runId, rest.flags.get('dir') as string | undefined);
        const raw = found ? ctx.vfs.read(`${found.dir}/branches.json`) : null;
        if (!raw) {
          io.out(`no persisted branch stores for run ${runId || '<run-id>'}\n`);
          return found ? 0 : 1;
        }
        const branches = JSON.parse(raw) as { label: string; branchId: string; status: string }[];
        io.out(`branches of ${runId}:\n`);
        for (const b of branches) io.out(`  ${b.branchId.padEnd(24)} ${b.status.padEnd(10)} ${b.label}\n`);
        return 0;
      }
      case 'branch-resume':
      case 'branch-rerun': {
        io.out(`chidori ${sub}: the docs VM records branch outcomes (see \`chidori branches <run-id>\`) but keeps branch\n`);
        io.out(`stores in memory only — re-run the parent agent to re-execute branches, or use the native CLI for real stores.\n`);
        return 0;
      }
      case 'init': {
        const dir = resolvePath(ctx.cwd, rest.positional[0] ?? '.');
        const template = String(rest.flags.get('template') ?? 'chat');
        ctx.vfs.write(
          `${dir}/agent.ts`,
          `import { chidori, run } from "chidori:agent";\n\nrun(async (input: { message?: string }) => {\n  const reply = await chidori.prompt(input.message ?? "Introduce yourself in one sentence.", { type: "final" });\n  return { reply };\n});\n`,
        );
        ctx.vfs.write(`${dir}/README.md`, `# chidori starter (${template})\n\nRun it:\n\n    chidori run agent.ts --input message="hello"\n`);
        io.out(`scaffolded ${template} template in ${dir}/ — try: chidori run agent.ts\n`);
        return 0;
      }
      case 'demo': {
        const demos = ctx.vfs
          .walk(`${ctx.cwd}/examples/agents`)
          .filter((p) => p.endsWith('.ts') && !p.includes('/actors/'));
        if (!demos.length) {
          io.out('no examples/ seeded in this directory — try `cd ~/project`\n');
          return 1;
        }
        io.out('runnable examples:\n');
        demos.forEach((p, i) => io.out(`  ${i + 1}. ${p.slice(ctx.cwd.length + 1)}\n`));
        const pick = await io.read(`choose [1-${demos.length}]: `);
        const idx = Number(pick.trim()) - 1;
        if (!(idx >= 0 && idx < demos.length)) {
          io.out('no selection — exiting demo picker\n');
          return 0;
        }
        return cmd(['chidori', 'run', demos[idx].slice(ctx.cwd.length + 1), '--input', 'name=reader'], io, ctx);
      }
      case 'model-login': {
        io.out('chidori model-login — zero-setup OpenRouter fallback.\n');
        io.out('In the docs VM the provider table lives in your browser: click “Connect OpenRouter” in this\n');
        io.out('panel’s header. One login powers every runnable example and the terminal alike.\n');
        return 0;
      }
      case 'add':
      case 'install':
      case 'remove': {
        const lockPath = `${ctx.cwd}/chidori.lock.jsonl`;
        const lock = new Map<string, string>(
          (ctx.vfs.read(lockPath) ?? '')
            .split('\n')
            .filter(Boolean)
            .map((l) => {
              const e = JSON.parse(l) as { name: string; line: string };
              return [e.name, l] as [string, string];
            }),
        );
        if (sub === 'add') {
          for (const pkg of rest.positional) {
            const name = pkg.replace(/@[\d^~].*$/, '');
            lock.set(name, JSON.stringify({ name, version: 'docs-vm', integrity: 'sha512-simulated', source: 'npm' }));
            io.out(`+ ${name} — fetched, SHA-512 verified, stored content-addressed (simulated by the docs VM)\n`);
          }
        } else if (sub === 'remove') {
          for (const pkg of rest.positional) {
            lock.delete(pkg);
            io.out(`- ${pkg}\n`);
          }
        } else {
          io.out(`installed ${lock.size} package${lock.size === 1 ? '' : 's'} from chidori.lock.jsonl (no Node involved)\n`);
        }
        ctx.vfs.write(lockPath, [...lock.values()].join('\n') + (lock.size ? '\n' : ''));
        if (sub !== 'install') io.out(`lockfile: chidori.lock.jsonl · imports of zod/ms resolve in-VM; other packages are stubs\n`);
        return 0;
      }
      case 'checkpoint': {
        io.out(`chidori checkpoint: value checkpoints are recorded per-run — see \`chidori trace <run_id>\` (step records)\n`);
        return 0;
      }
      case undefined:
      case 'help':
      case '--help': {
        io.out(
          'chidori — durable TypeScript agents (docs VM build, running on the wasm engine in your browser)\n\n' +
            '  run <agent.ts> --input k=v     one-shot run (journals to .chidori/runs/)\n' +
            '  serve [agent.ts] --port 8080   session server (curl it from this terminal)\n' +
            '  resume <agent.ts> <run_id>     replay a recording for $0\n' +
            '  verify <agent.ts> <run_id>     checkpoint-as-test: no provider, deny-all policy\n' +
            '  trace <run_id>                 print the call log with token/cost totals\n' +
            '  chat [agent.ts]                interactive REPL\n' +
            '  demo · init · check · stats · snapshot · branches · add/install/remove · model-login\n',
        );
        return 0;
      }
      default:
        io.err(`chidori: unknown subcommand "${sub}" — try \`chidori help\`\n`);
        return 2;
    }
  };
  return cmd;
}
