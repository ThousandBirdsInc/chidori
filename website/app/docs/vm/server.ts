'use client';

/**
 * The docs VM's "localhost": an in-page session server that `chidori serve`
 * registers on a port and the VM's `curl` routes to. Sessions are real runs
 * on the wasm engine, held live in the page — `chidori.input()` pauses them
 * (resume with POST /sessions/{id}/resume), `chidori.signal()` listen
 * points accept POST /sessions/{id}/signal — the exact flow the Getting
 * Started and Signals pages walk through.
 */

import type { Json } from '../../(home)/playground/brain';
import { buildHarnessSource } from '../runner/harness';
import { createRunHost, type EngineAssets, type SignalRequest } from '../runner/run-host';
import { fakeOutput, fakePendingPrompt } from './fake-cli';
import { basename, dirname, type Vfs } from './vfs';

interface Session {
  id: string;
  status: 'running' | 'paused' | 'completed' | 'failed';
  pendingPrompt: string | null;
  pendingDetails: Json;
  resolveInput: ((answer: string) => void) | null;
  pendingSignals: SignalRequest[];
  output: Json;
  error: string | null;
  deliver: (msg: { name: string | null; payload: Json; from: { kind: string; id: string } | null }) => void;
  settled: Promise<void>;
}

interface Server {
  port: number;
  agentPath: string;
  fleetOnly: boolean;
  sessions: Map<string, Session>;
  counter: number;
}

const servers = new Map<number, Server>();

export function startServer(port: number, agentPath: string | null): { alreadyRunning: boolean } {
  const existing = servers.get(port);
  if (existing) return { alreadyRunning: true };
  servers.set(port, {
    port,
    agentPath: agentPath ?? '',
    fleetOnly: agentPath === null,
    sessions: new Map(),
    counter: 0,
  });
  return { alreadyRunning: false };
}

export function stopServer(port: number): boolean {
  return servers.delete(port);
}

export function listServers(): { port: number; agentPath: string; sessions: number }[] {
  return [...servers.values()].map((s) => ({ port: s.port, agentPath: s.agentPath, sessions: s.sessions.size }));
}

function sessionView(s: Session): Json {
  return {
    id: s.id,
    status: s.status,
    ...(s.pendingPrompt !== null ? { pending_prompt: s.pendingPrompt } : {}),
    ...(s.pendingDetails !== null && s.pendingDetails !== undefined ? { pending_details: s.pendingDetails } : {}),
    ...(s.output !== null ? { output: s.output } : {}),
    ...(s.error !== null ? { error: s.error } : {}),
  };
}

const json = (status: number, body: Json): { status: number; body: string } => ({
  status,
  body: JSON.stringify(body, null, 2),
});

/**
 * Route an in-VM HTTP request. Returns null when nothing listens on the
 * port (curl then prints connection refused).
 */
export async function routeRequest(
  deps: { vfs: Vfs; engine: () => Promise<EngineAssets> },
  method: string,
  rawUrl: string,
  body: string | null,
): Promise<{ status: number; body: string } | null> {
  const url = new URL(rawUrl);
  const server = servers.get(Number(url.port || 80));
  if (!server) return null;
  const path = url.pathname.replace(/\/$/, '');
  let parsed: Json = null;
  try {
    parsed = body ? (JSON.parse(body) as Json) : null;
  } catch {
    parsed = body;
  }

  if (method === 'POST' && path === '/sessions') {
    if (server.fleetOnly) {
      return json(400, { error: 'this server hosts detached agents only — sessions must name an agent' });
    }
    const input = parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? ((parsed as Record<string, Json>).input ?? {}) : {};
    const session = await createSession(deps, server, input);
    // Give the run a beat to reach its first pause (or completion).
    await Promise.race([session.settled, new Promise((r) => setTimeout(r, 4000))]);
    return json(session.status === 'failed' ? 500 : 200, sessionView(session));
  }

  const m = path.match(/^\/sessions\/([^/]+)(?:\/(resume|signal))?$/);
  if (!m) return json(404, { error: `no such route: ${method} ${path}` });
  const session = server.sessions.get(m[1]);
  if (!session) return json(404, { error: `no such session: ${m[1]}` });

  if (method === 'GET' && !m[2]) return json(200, sessionView(session));

  if (method === 'POST' && m[2] === 'resume') {
    if (!session.resolveInput) return json(409, { error: `session is ${session.status}, not paused at input()` });
    const response =
      parsed && typeof parsed === 'object' && !Array.isArray(parsed)
        ? String((parsed as Record<string, Json>).response ?? '')
        : String(parsed ?? '');
    const resolve = session.resolveInput;
    session.resolveInput = null;
    session.pendingPrompt = null;
    session.pendingDetails = null;
    session.status = 'running';
    resolve(response);
    await Promise.race([session.settled, new Promise((r) => setTimeout(r, 4000))]);
    return json(200, sessionView(session));
  }

  if (method === 'POST' && m[2] === 'signal') {
    const obj = parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? (parsed as Record<string, Json>) : {};
    const from = (obj.from as { kind: string; id: string } | undefined) ?? { kind: 'human', id: 'curl' };
    session.deliver({ name: String(obj.name ?? ''), payload: obj.payload ?? null, from });
    // Let a resumed listen point advance before reporting state.
    await new Promise((r) => setTimeout(r, 300));
    return json(200, { delivered: true, session: sessionView(session) });
  }

  return json(405, { error: `unsupported: ${method} ${path}` });
}

async function createSession(
  deps: { vfs: Vfs; engine: () => Promise<EngineAssets> },
  server: Server,
  input: Json,
): Promise<Session> {
  const engine = await deps.engine().catch(() => null);
  const id = `sess-${(++server.counter).toString().padStart(3, '0')}-${Math.random().toString(36).slice(2, 8)}`;
  const session: Session = {
    id,
    status: 'running',
    pendingPrompt: null,
    pendingDetails: null,
    resolveInput: null,
    pendingSignals: [],
    output: null,
    error: null,
    deliver: () => {},
    settled: Promise.resolve(),
  };
  server.sessions.set(id, session);

  const code = deps.vfs.read(server.agentPath);
  if (code === null) {
    session.status = 'failed';
    session.error = `agent file not found: ${server.agentPath}`;
    return session;
  }

  if (engine === null) {
    // No wasm engine: fake the session. It pauses at the file's first
    // input() prompt and completes on resume with a scripted output — the
    // documented pause/resume flow, honestly labelled.
    const pending = fakePendingPrompt(code);
    const inputObj = input && typeof input === 'object' && !Array.isArray(input) ? (input as Record<string, Json>) : {};
    if (pending) {
      session.status = 'paused';
      session.pendingPrompt = pending;
      session.settled = new Promise<void>((settle) => {
        session.resolveInput = (answer: string) => {
          session.status = 'completed';
          session.output = {
            ...(fakeOutput(basename(server.agentPath), inputObj, [answer], []) as Record<string, Json>),
            faked: true,
          };
          settle();
        };
      });
    } else {
      session.status = 'completed';
      session.output = { ...(fakeOutput(basename(server.agentPath), inputObj, [], []) as Record<string, Json>), faked: true };
    }
    return session;
  }

  const host = createRunHost({
    vfs: deps.vfs,
    projectDir: dirname(server.agentPath),
    engine,
    trusted: true, // serve-mode approvals are HTTP routes; the docs VM allows
    ui: {
      askInput: (payload) =>
        new Promise<string>((resolve) => {
          session.status = 'paused';
          session.pendingPrompt = payload.prompt;
          const opts = payload.opts && typeof payload.opts === 'object' && !Array.isArray(payload.opts) ? (payload.opts as Record<string, Json>) : {};
          session.pendingDetails = opts.details ?? null;
          session.resolveInput = resolve;
        }),
      waitSignal: (req) => {
        session.pendingSignals.push(req);
      },
      signalDone: (req) => {
        session.pendingSignals = session.pendingSignals.filter((r) => r !== req);
      },
    },
  });
  session.deliver = (msg) => host.deliver(msg);

  session.settled = (async () => {
    try {
      const agent = engine.sdk.BrowserAgent.start(engine.wasm, {
        source: buildHarnessSource(code, input),
        ...host.hostOptions,
      });
      const result = await agent.run();
      let output: Json = null;
      for (const line of result.console) {
        try {
          const ev = JSON.parse(line);
          if (ev && ev.k === 'result') output = ev.value ?? null;
          if (ev && ev.k === 'error') throw new Error(String(ev.text));
        } catch (err) {
          if (err instanceof Error && !(err instanceof SyntaxError)) throw err;
        }
      }
      session.status = 'completed';
      session.output = output;
    } catch (err) {
      session.status = 'failed';
      session.error = String(err instanceof Error ? err.message : err);
    }
  })();

  return session;
}
