'use client';

/**
 * The docs example runner: a right-side panel, mounted once in the docs
 * layout, that executes the code block the reader clicked "Run" on.
 *
 * ts/js blocks run for real on the wasm build of the chidori engine in this
 * tab (the same engine + browser SDK the /playground chat uses), with the
 * full documented `chidori.*` surface recreated by harness.ts + run-host.ts
 * — prompts, tool loops, signals the reader delivers interactively, actors
 * spawned as real nested runs, workspace files shared with the VM
 * filesystem, memory that persists across runs. Shell blocks open the docs
 * VM instead: a simulated Linux terminal whose `chidori` CLI runs agent
 * files on the same engine.
 *
 * Prompts are served by the site-wide OpenRouter login (lib/openrouter.ts)
 * when connected — one login covers every example, the terminal, and the
 * playground — and by a deterministic offline reply otherwise. The feed
 * renders purely from the run's journaled console, which is what makes
 * "Replay offline" repaint the whole run with zero live calls.
 */

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react';
import {
  completeOpenRouterLogin,
  getOpenRouterKey,
  getOpenRouterModel,
  setOpenRouterKey,
  setOpenRouterModel,
  startOpenRouterLogin,
  subscribeOpenRouter,
} from '@/lib/openrouter';
import type { Json } from '../../(home)/playground/brain';
import { scriptFromBlock, VmTerminal } from '../vm/terminal';
import { PROJECT, loadVfs } from '../vm/vfs';
import { loadDocsIndex, loadEngine } from './assets';
import { buildHarnessSource } from './harness';
import {
  OFFLINE_REPLY,
  type RunEvent,
  makeDocsTools,
  parseRunFeed,
} from './host';
import { createRunHost, type AgentHandle, type DocsRunHost, type SignalRequest } from './run-host';
import {
  closeRunner,
  getRunnerExample,
  subscribeRunner,
} from './runner-store';

interface PendingInput {
  prompt: string;
  opts: Record<string, Json>;
  resolve: (answer: string) => void;
}

type Phase = 'idle' | 'starting' | 'running' | 'done' | 'failed';

const short = (value: Json, max = 2000): string => {
  const text = JSON.stringify(value, null, 1) ?? 'null';
  return text.length > max ? `${text.slice(0, max)}…` : text;
};

/** Strip event handlers/scripts from renderDOM output before display. */
function sanitizeDomHtml(html: string): string {
  return html
    .replace(/<\s*(script|iframe|object|embed|link|meta)[\s\S]*?(<\/\s*\1\s*>|\/?>)/gi, '')
    .replace(/\son\w+\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)/gi, '')
    .replace(/(href|src)\s*=\s*("javascript:[^"]*"|'javascript:[^']*')/gi, '');
}

function EventCard({ event }: { event: RunEvent }) {
  const label = 'font-mono text-[10px] uppercase tracking-wide text-fd-muted-foreground';
  const card = 'rounded-lg border border-fd-border bg-fd-card px-3 py-2 text-xs';
  switch (event.k) {
    case 'log':
      return (
        <div className={card}>
          <p className={label}>chidori.log</p>
          <p className="mt-1">{event.message}</p>
          {event.fields !== null && event.fields !== undefined && (
            <pre className="mt-1 max-h-32 overflow-auto font-mono text-[11px] text-fd-muted-foreground">{short(event.fields)}</pre>
          )}
        </div>
      );
    case 'prompt':
      return (
        <div className={card}>
          <p className={label}>
            chidori.prompt{event.toolTurns ? ` · ${event.toolTurns} tool turn${event.toolTurns === 1 ? '' : 's'}` : ''}
          </p>
          <p className="mt-1 whitespace-pre-wrap text-fd-muted-foreground">{event.text.length > 280 ? `${event.text.slice(0, 280)}…` : event.text}</p>
          <p className="mt-2 whitespace-pre-wrap border-t border-fd-border pt-2">
            {event.reply === OFFLINE_REPLY ? <em className="text-fd-muted-foreground">{event.reply}</em> : event.reply}
          </p>
        </div>
      );
    case 'tool':
      return (
        <div className={card}>
          <p className={label}>tool · {event.name}</p>
          <pre className="mt-1 max-h-24 overflow-auto font-mono text-[11px] text-fd-muted-foreground">{short(event.args, 600)}</pre>
          <pre className="mt-1 max-h-32 overflow-auto border-t border-fd-border pt-1 font-mono text-[11px]">{short(event.result)}</pre>
        </div>
      );
    case 'input':
      return (
        <div className={card}>
          <p className={label}>chidori.input · answered</p>
          <p className="mt-1">{event.prompt}</p>
          <p className="mt-1 font-medium">→ {event.answer}</p>
        </div>
      );
    case 'signal': {
      if (event.phase === 'waiting' || event.phase === 'receiving') return null; // the live wait renders its own card
      const what = event.phase === 'timeout' ? 'timed out' : event.phase === 'poll-empty' ? 'poll → nothing queued' : 'received';
      return (
        <div className={card}>
          <p className={label}>
            {event.phase === 'poll-empty' ? 'chidori.pollSignal' : 'signal'} · {event.names.join(' | ')} · {what}
          </p>
          {event.result !== undefined && event.phase === 'received' && (
            <pre className="mt-1 max-h-32 overflow-auto font-mono text-[11px]">{short(event.result, 600)}</pre>
          )}
        </div>
      );
    }
    case 'op':
      return (
        <div className={card}>
          <p className={label}>{event.op}</p>
          <p className="mt-1 break-words font-mono text-[11px]">{event.label}</p>
          {event.data !== null && event.data !== undefined && (
            <pre className="mt-1 max-h-32 overflow-auto font-mono text-[11px] text-fd-muted-foreground">{short(event.data, 800)}</pre>
          )}
        </div>
      );
    case 'dom':
      return (
        <div className={`${card} border-fd-primary/40`}>
          <p className={label}>chidori.renderDOM · {event.ops} mutation{event.ops === 1 ? '' : 's'} flushed</p>
          <div
            className="mt-2 rounded border border-dashed border-fd-border bg-fd-background p-2"
            // Rendered output of the example's virtual DOM, sanitized above.
            dangerouslySetInnerHTML={{ __html: sanitizeDomHtml(event.html) }}
          />
        </div>
      );
    case 'fetch':
      return (
        <p className="font-mono text-[11px] text-fd-muted-foreground">
          ⇄ fetch {event.url.length > 64 ? `${event.url.slice(0, 64)}…` : event.url} → {event.status}
          {event.simulated ? ' · simulated (live request failed)' : ''}
        </p>
      );
    case 'result':
      return (
        <div className={`${card} border-fd-primary/40`}>
          <p className={label}>run output</p>
          <pre className="mt-1 max-h-48 overflow-auto font-mono text-[11px]">{short(event.value)}</pre>
        </div>
      );
    case 'error':
      return (
        <div className={`${card} border-red-500/50`}>
          <p className={`${label} text-red-500`}>error</p>
          <p className="mt-1 whitespace-pre-wrap font-mono text-[11px]">{event.text}</p>
        </div>
      );
    case 'done':
      return <p className="text-center font-mono text-[11px] text-fd-muted-foreground">— run completed —</p>;
    default:
      return <p className="font-mono text-[11px] text-fd-muted-foreground">{event.text}</p>;
  }
}

/** The interactive delivery card for a run paused at a signal listen point. */
function SignalCard({ req }: { req: SignalRequest }) {
  const [name, setName] = useState(req.names[0] ?? '');
  const [payload, setPayload] = useState('{}');
  const [error, setError] = useState('');
  const action =
    'inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-fd-border px-2.5 text-xs font-medium transition-colors hover:bg-fd-accent';
  return (
    <div className="rounded-lg border border-fd-primary/50 bg-fd-card px-3 py-2.5 text-xs" data-runner-signal>
      <p className="font-mono text-[10px] uppercase tracking-wide text-fd-muted-foreground">
        {req.mode === 'receive' ? 'chidori.receive' : 'chidori.signal'} · {req.who} is paused, listening
      </p>
      <p className="mt-1.5 text-sm">
        Waiting for <strong>{req.names.join(' | ')}</strong>
        {req.timeoutMs !== null ? ` (timeout ${req.timeoutMs} ms)` : ''} — you are the outside party. Deliver one:
      </p>
      <div className="mt-2 flex flex-wrap gap-1.5">
        {req.names.map((n) => (
          <button key={n} className={`${action} ${n === name ? 'border-fd-primary/60' : ''}`} onClick={() => setName(n)}>
            {n}
          </button>
        ))}
      </div>
      <textarea
        value={payload}
        onChange={(e) => setPayload(e.target.value)}
        rows={2}
        spellCheck={false}
        aria-label="Signal payload (JSON)"
        className="mt-2 w-full resize-y rounded-lg border border-fd-border bg-fd-background p-2 font-mono text-[11px] outline-none focus:ring-2 focus:ring-fd-primary/40"
      />
      {error && <p className="mt-1 text-red-500">{error}</p>}
      <div className="mt-2 flex gap-1.5">
        <button
          className={`${action} border-fd-primary/60`}
          onClick={() => {
            try {
              const parsed = payload.trim() ? (JSON.parse(payload) as Json) : null;
              req.deliver({ name, payload: parsed, from: { kind: 'human', id: 'you' } });
            } catch (err) {
              setError(`payload is not valid JSON: ${String(err)}`);
            }
          }}
        >
          Deliver {name}
        </button>
        {req.timeoutMs !== null && (
          <button className={action} onClick={() => req.fireTimeout()} title="Skip the wait — resolve as { timedOut: true }">
            ⏭ Let it time out
          </button>
        )}
      </div>
    </div>
  );
}

export function RunnerPanel() {
  const example = useSyncExternalStore(subscribeRunner, getRunnerExample, () => null);
  const orKey = useSyncExternalStore(subscribeOpenRouter, getOpenRouterKey, () => null);
  const [model, setModel] = useState('openrouter/auto');
  const [phase, setPhase] = useState<Phase>('idle');
  const [busy, setBusy] = useState<string | null>(null);
  const [feed, setFeed] = useState<RunEvent[]>([]);
  const [statusLine, setStatusLine] = useState('');
  const [inputJson, setInputJson] = useState('{}');
  const [pending, setPending] = useState<PendingInput | null>(null);
  const [signals, setSignals] = useState<SignalRequest[]>([]);
  const [answerDraft, setAnswerDraft] = useState('');
  const [hasRecording, setHasRecording] = useState(false);
  const [termNonce, setTermNonce] = useState(0);

  // Orphans stale agents: host calls from a superseded run hang forever.
  const tokenRef = useRef(0);
  const agentRef = useRef<AgentHandle | null>(null);
  const hostRef = useRef<DocsRunHost | null>(null);
  const feedBoxRef = useRef<HTMLDivElement | null>(null);
  const exampleIdRef = useRef<string | null>(null);

  // Finish the OpenRouter PKCE login if this page load is the redirect back
  // from openrouter.ai (no-op otherwise) — docs pages are valid callback
  // targets, so a login started from any example lands back where it began.
  useEffect(() => {
    setModel(getOpenRouterModel());
    completeOpenRouterLogin().catch((err) => {
      setStatusLine(`OpenRouter login failed: ${String(err)}`);
    });
  }, []);

  const reset = useCallback(() => {
    tokenRef.current += 1;
    hostRef.current?.cancel();
    hostRef.current = null;
    agentRef.current = null;
    setPhase('idle');
    setBusy(null);
    setFeed([]);
    setPending(null);
    setSignals([]);
    setAnswerDraft('');
    setStatusLine('');
    setHasRecording(false);
  }, []);

  // A different example was opened: drop any run state and prefill its input.
  useEffect(() => {
    const id = example?.id ?? null;
    if (id === exampleIdRef.current) return;
    exampleIdRef.current = id;
    reset();
    if (example) setInputJson(JSON.stringify(example.input ?? {}, null, 2));
    if (example?.mode === 'shell') setTermNonce((n) => n + 1);
  }, [example, reset]);

  useEffect(() => {
    if (!example) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeRunner();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [example]);

  useEffect(() => {
    const box = feedBoxRef.current;
    if (box) box.scrollTop = box.scrollHeight;
  }, [feed, pending, signals, busy]);

  const refreshFeed = useCallback(() => {
    const agent = agentRef.current;
    if (agent) setFeed(parseRunFeed(agent.console()));
  }, []);

  const start = useCallback(async () => {
    if (!example || example.mode === 'shell') return;
    let input: unknown = {};
    if (example.mode === 'program') {
      try {
        input = JSON.parse(inputJson || '{}');
      } catch (err) {
        setStatusLine(`Run input is not valid JSON: ${String(err)}`);
        return;
      }
    }
    tokenRef.current += 1;
    const token = tokenRef.current;
    const stale = () => token !== tokenRef.current;
    const hang = () => new Promise<never>(() => {});
    hostRef.current?.cancel();
    agentRef.current = null;
    setFeed([]);
    setPending(null);
    setSignals([]);
    setStatusLine('');
    setHasRecording(false);
    setPhase('starting');
    setBusy('loading the wasm engine…');
    let engine: Awaited<ReturnType<typeof loadEngine>>;
    try {
      engine = await loadEngine();
    } catch {
      setPhase('failed');
      setBusy(null);
      setStatusLine('The wasm assets are missing. Build them with scripts/build-wasm.sh, then reload.');
      return;
    }
    const vfs = await loadVfs();
    const docsIndex = await loadDocsIndex();
    if (stale()) return;

    const host = createRunHost({
      vfs,
      projectDir: PROJECT,
      engine,
      trusted: true, // panel runs auto-approve; the VM terminal demos y/a/N gates
      getDocsTools: () => makeDocsTools(() => docsIndex),
      ui: {
        refresh: () => {
          if (!stale()) refreshFeed();
        },
        busy: (text) => {
          if (!stale()) setBusy(text ? text.replace(/^run: /, '') : null);
        },
        note: (text) => {
          if (!stale()) setStatusLine(text);
        },
        askInput: (payload) => {
          if (stale()) return hang();
          setBusy(null);
          refreshFeed();
          const opts = (payload.opts ?? {}) as Record<string, Json>;
          setAnswerDraft(String(opts.default ?? ''));
          return new Promise<string>((resolve) => {
            setPending({ prompt: payload.prompt, opts, resolve });
          });
        },
        waitSignal: (req) => {
          if (stale()) return;
          setBusy(null);
          refreshFeed();
          setSignals((prev) => [...prev, req]);
        },
        signalDone: (req) => {
          setSignals((prev) => prev.filter((r) => r !== req));
        },
      },
    });
    hostRef.current = host;

    try {
      const agent = engine.sdk.BrowserAgent.start(engine.wasm, {
        source: buildHarnessSource(example.code, input, example.ambient),
        ...host.hostOptions,
      }) as AgentHandle;
      agentRef.current = agent;
      setPhase('running');
      setBusy('running…');
      const result = await agent.run();
      if (stale()) return;
      setBusy(null);
      setPending(null);
      setSignals([]);
      setFeed(parseRunFeed(result.console));
      setHasRecording(true);
      setPhase('done');
      setStatusLine(
        `⚡ ${result.console.length} journaled event${result.console.length === 1 ? '' : 's'} — saved as one durable blob; replay it offline below.`,
      );
    } catch (err) {
      if (stale()) return;
      setBusy(null);
      setPending(null);
      setSignals([]);
      refreshFeed();
      setPhase('failed');
      setStatusLine(`Run failed: ${String(err)}`);
    }
  }, [example, inputJson, refreshFeed]);

  const answer = useCallback(
    (text: string) => {
      if (!pending) return;
      setPending(null);
      setBusy('running…');
      pending.resolve(text);
    },
    [pending],
  );

  /** Re-render the whole run from its journal: no model, no network. */
  const replay = useCallback(async () => {
    const agent = agentRef.current;
    if (!agent) return;
    try {
      const engine = await loadEngine();
      const replayed = engine.sdk.BrowserAgent.restore(engine.wasm, agent.blob(), {
        llm: () => {
          throw new Error('replay must not call the LLM');
        },
        fetchImpl: (() => {
          throw new Error('replay must not touch the network');
        }) as unknown as typeof fetch,
      }) as AgentHandle;
      const result = await replayed.run();
      setFeed(parseRunFeed(result.console));
      setStatusLine(
        `⚡ Replayed offline: ${result.console.length} journaled events re-rendered with ${result.liveCalls} live calls.`,
      );
    } catch (err) {
      setStatusLine(`Replay error: ${String(err)}`);
    }
  }, []);

  if (!example) return null;

  const isShell = example.mode === 'shell';
  const action =
    'inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-fd-border px-2.5 text-xs font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';
  const choices =
    pending && Array.isArray(pending.opts.choices)
      ? (pending.opts.choices as Json[]).map((c) => String(c))
      : [];
  const details = pending?.opts.details;
  const running = phase === 'starting' || phase === 'running';

  return (
    <aside
      id="example-runner"
      aria-label="Runnable example"
      className="fixed inset-y-0 right-0 z-50 flex w-full flex-col border-l border-fd-border bg-fd-background shadow-2xl sm:w-[28rem]"
    >
      <div className="flex items-center gap-2 border-b border-fd-border px-3 py-2.5">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" aria-hidden>
          <path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z" />
        </svg>
        <div className="min-w-0">
          <p className="truncate text-sm font-medium">{isShell ? 'Docs VM terminal' : 'Run this example'}</p>
          <p className="truncate text-[11px] text-fd-muted-foreground">
            {example.title} · {isShell ? 'simulated Linux, real wasm chidori CLI' : 'live on the wasm engine in this tab'}
          </p>
        </div>
        <button
          id="runner-close"
          className={`${action} ml-auto`}
          onClick={closeRunner}
          aria-label="Close the example runner"
        >
          ✕
        </button>
      </div>

      {/* The global OpenRouter connection: one login serves every runnable
          example on the site (and the /playground chat). */}
      <div className="flex flex-wrap items-center gap-2 border-b border-fd-border bg-fd-accent/30 px-3 py-2">
        {orKey ? (
          <>
            <span id="runner-or-connected" className="shrink-0 text-xs text-fd-muted-foreground">
              ✓ OpenRouter
            </span>
            <input
              id="runner-or-model"
              type="text"
              value={model}
              onChange={(e) => {
                setModel(e.target.value);
                setOpenRouterModel(e.target.value);
              }}
              aria-label="OpenRouter model"
              className="h-8 w-24 min-w-0 flex-1 rounded-lg border border-fd-border bg-fd-background px-2 text-xs"
            />
            <button id="runner-or-disconnect" className={action} onClick={() => setOpenRouterKey(null)}>
              Disconnect
            </button>
          </>
        ) : (
          <>
            <button id="runner-or-connect" className={action} onClick={() => void startOpenRouterLogin()}>
              Connect OpenRouter
            </button>
            <span className="min-w-0 flex-1 text-[11px] leading-tight text-fd-muted-foreground">
              One login runs every docs example{isShell ? ' — including `chidori run` in this terminal' : ' (and the playground)'}. Until
              then, prompts return a deterministic offline reply.
            </span>
          </>
        )}
      </div>

      {isShell ? (
        <div className="min-h-0 flex-1 p-2.5">
          <VmTerminal key={`${example.id}-${termNonce}`} script={scriptFromBlock(example.code)} />
        </div>
      ) : (
        <div ref={feedBoxRef} className="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-contain p-3">
          <details className="rounded-lg border border-fd-border bg-fd-card/50">
            <summary className="cursor-pointer select-none px-3 py-2 text-xs font-medium">
              The code this runs{example.mode === 'fragment' ? ' (wrapped in an async run for you)' : ''}
            </summary>
            {example.ambient && (
              <pre className="max-h-32 overflow-auto border-t border-fd-border p-3 font-mono text-[11px] text-fd-muted-foreground">{`// stand-ins for names the surrounding docs establish:\n${example.ambient}`}</pre>
            )}
            <pre className="max-h-56 overflow-auto border-t border-fd-border p-3 font-mono text-[11px]">{example.code}</pre>
          </details>

          {example.mode === 'program' && (
            <div>
              <label htmlFor="runner-input" className="text-[11px] font-medium text-fd-muted-foreground">
                Run input — the JSON handed to this agent&apos;s run() handler
              </label>
              <textarea
                id="runner-input"
                value={inputJson}
                onChange={(e) => setInputJson(e.target.value)}
                rows={Math.min(6, inputJson.split('\n').length)}
                spellCheck={false}
                className="mt-1 w-full resize-y rounded-lg border border-fd-border bg-fd-background p-2 font-mono text-xs outline-none focus:ring-2 focus:ring-fd-primary/40"
              />
            </div>
          )}

          {feed.length === 0 && !busy && (
            <p className="text-xs leading-relaxed text-fd-muted-foreground">
              Press <strong>Run</strong> and this code executes for real — the pure-Rust chidori engine
              compiled to WebAssembly, right here in this tab. Every <code>chidori.*</code> call is
              journaled as it happens, so when the run finishes you can replay it offline,
              byte-identically, with zero live calls. Signals pause for <em>you</em> to deliver; actors
              and branches spawn real nested runs; workspace files land in the docs VM&apos;s filesystem.
            </p>
          )}

          {feed.map((event, i) => (
            <EventCard key={i} event={event} />
          ))}

          {signals.map((req, i) => (
            <SignalCard key={`${req.names.join('|')}-${i}`} req={req} />
          ))}

          {pending && (
            <div className="rounded-lg border border-fd-primary/50 bg-fd-card px-3 py-2.5 text-xs" id="runner-pending-input">
              <p className="font-mono text-[10px] uppercase tracking-wide text-fd-muted-foreground">
                chidori.input · the run is suspended, waiting on you
              </p>
              <p className="mt-1.5 text-sm font-medium">{pending.prompt}</p>
              {details !== null && details !== undefined && (
                <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap rounded border border-fd-border bg-fd-background p-2 font-mono text-[11px]">
                  {typeof details === 'string' ? details : short(details)}
                </pre>
              )}
              {choices.length > 0 ? (
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {choices.map((choice) => (
                    <button
                      key={choice}
                      id={`runner-choice-${choice.replace(/\s+/g, '-')}`}
                      className={`${action} ${choice === String(pending.opts.default ?? '') ? 'border-fd-primary/60' : ''}`}
                      onClick={() => answer(choice)}
                    >
                      {choice}
                    </button>
                  ))}
                </div>
              ) : (
                <form
                  className="mt-2 flex gap-1.5"
                  onSubmit={(e) => {
                    e.preventDefault();
                    answer(answerDraft);
                  }}
                >
                  <input
                    id="runner-answer"
                    type="text"
                    value={answerDraft}
                    onChange={(e) => setAnswerDraft(e.target.value)}
                    autoComplete="off"
                    className="h-8 min-w-0 flex-1 rounded-lg border border-fd-border bg-fd-background px-2 text-xs outline-none focus:ring-2 focus:ring-fd-primary/40"
                  />
                  <button type="submit" className={action}>
                    Answer
                  </button>
                </form>
              )}
            </div>
          )}

          {busy && (
            <p className="flex items-center gap-2 font-mono text-[11px] text-fd-muted-foreground" id="runner-busy">
              <span aria-hidden className="flex gap-1">
                <span className="size-1 animate-pulse rounded-full bg-current" />
                <span className="size-1 animate-pulse rounded-full bg-current [animation-delay:200ms]" />
                <span className="size-1 animate-pulse rounded-full bg-current [animation-delay:400ms]" />
              </span>
              {busy}
            </p>
          )}
        </div>
      )}

      {statusLine && !isShell && (
        <p
          id="runner-status"
          className="border-t border-fd-border bg-fd-accent/30 px-3 py-1.5 font-mono text-[11px] leading-relaxed text-fd-muted-foreground"
        >
          {statusLine}
        </p>
      )}
      <div className="flex items-center gap-1.5 border-t border-fd-border p-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))]">
        {isShell ? (
          <>
            <button
              id="runner-run"
              className="h-9 shrink-0 rounded-lg bg-fd-primary px-4 text-sm font-medium text-fd-primary-foreground transition-opacity hover:opacity-85"
              onClick={() => setTermNonce((n) => n + 1)}
              title="Reboot the terminal and replay this block's commands"
            >
              ↻ Replay commands
            </button>
            <span className="min-w-0 flex-1 truncate text-[11px] text-fd-muted-foreground">
              The prompt is yours when playback ends — files persist for this tab.
            </span>
          </>
        ) : (
          <>
            <button
              id="runner-run"
              className="h-9 shrink-0 rounded-lg bg-fd-primary px-4 text-sm font-medium text-fd-primary-foreground transition-opacity hover:opacity-85 disabled:pointer-events-none disabled:opacity-40"
              disabled={running}
              onClick={() => void start()}
            >
              {phase === 'done' || phase === 'failed' ? '▶ Run again' : '▶ Run'}
            </button>
            <button
              id="runner-replay"
              className={action}
              disabled={!hasRecording || running}
              onClick={() => void replay()}
              title="Re-render this run from its journal — zero live calls"
            >
              ↺ Replay offline
            </button>
            <button
              id="runner-clear"
              className={`${action} ml-auto`}
              disabled={feed.length === 0 && !statusLine && !running}
              onClick={reset}
            >
              {running ? '■ Stop' : 'Clear'}
            </button>
          </>
        )}
      </div>
    </aside>
  );
}
