'use client';

/**
 * The docs example runner: a right-side panel, mounted once in the docs
 * layout, that executes the code block the reader clicked "Run" on — for
 * real, on the wasm build of the chidori engine, in this tab (the same
 * engine + browser SDK the /playground chat uses).
 *
 * The harness (harness.ts) recreates the `chidori:agent` surface the docs'
 * durable-core examples use; the host half lives in host.ts. Prompts are
 * served by the site-wide OpenRouter login (lib/openrouter.ts) when
 * connected — one login covers every example and the playground — and by a
 * deterministic offline reply otherwise. The feed renders purely from the
 * run's journaled console, which is what makes "Replay offline" repaint the
 * whole run with zero live calls.
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
import { type DocsIndex, type Json, prepareDocsIndex } from '../../(home)/playground/brain';
import { buildHarnessSource } from './harness';
import {
  OFFLINE_REPLY,
  type RunEvent,
  decidePrompt,
  fetchWithSimulatedFallback,
  makeDocsTools,
  parseRunFeed,
} from './host';
import {
  type RunnableExample,
  closeRunner,
  getRunnerExample,
  subscribeRunner,
} from './runner-store';

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
const ASSETS = `${BASE}/chidori-wasm`;

interface RunResult {
  status: string;
  console: string[];
  liveCalls: number;
  pendingInput?: { prompt: string; opts?: Json };
}

interface AgentHandle {
  run(): Promise<RunResult>;
  console(): string[];
  blob(): Uint8Array;
}

interface Loaded {
  wasm: unknown;
  sdk: typeof import('../../../../sdk/browser/index.js');
}

let assetsPromise: Promise<Loaded> | null = null;

function loadAssets(): Promise<Loaded> {
  // Runtime imports on purpose (same as the playground): the wasm module and
  // SDK are static assets built by scripts/build-wasm.sh, not bundle modules.
  assetsPromise ??= (async () => {
    const wasm = await import(/* webpackIgnore: true */ `${ASSETS}/chidori_wasm.js`);
    await wasm.default();
    const sdk = await import(/* webpackIgnore: true */ `${ASSETS}/chidori-browser.js`);
    return { wasm, sdk };
  })();
  return assetsPromise;
}

let docsIndexPromise: Promise<DocsIndex | null> | null = null;

function loadDocsIndex(): Promise<DocsIndex | null> {
  docsIndexPromise ??= fetch(`${BASE}/playground-docs.json`)
    .then((res) => (res.ok ? res.json() : null))
    .then((json) => (json ? prepareDocsIndex(json) : null))
    .catch(() => null);
  return docsIndexPromise;
}

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
  const [answerDraft, setAnswerDraft] = useState('');
  const [hasRecording, setHasRecording] = useState(false);

  // Orphans stale agents: host calls from a superseded run hang forever.
  const tokenRef = useRef(0);
  const agentRef = useRef<AgentHandle | null>(null);
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
    agentRef.current = null;
    setPhase('idle');
    setBusy(null);
    setFeed([]);
    setPending(null);
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
  }, [feed, pending, busy]);

  const refreshFeed = useCallback(() => {
    const agent = agentRef.current;
    if (agent) setFeed(parseRunFeed(agent.console()));
  }, []);

  const start = useCallback(async () => {
    if (!example) return;
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
    agentRef.current = null;
    setFeed([]);
    setPending(null);
    setStatusLine('');
    setHasRecording(false);
    setPhase('starting');
    setBusy('loading the wasm engine…');
    let loaded: Loaded;
    try {
      loaded = await loadAssets();
    } catch {
      setPhase('failed');
      setBusy(null);
      setStatusLine('The wasm assets are missing. Build them with scripts/build-wasm.sh, then reload.');
      return;
    }
    const docsIndex = await loadDocsIndex();
    if (stale()) return;

    const baseTools = makeDocsTools(() => docsIndex);
    const tools: Record<string, (kwargs: Json) => Promise<Json>> = {};
    for (const [name, impl] of Object.entries(baseTools)) {
      tools[name] = async (kwargs) => {
        if (stale()) return hang();
        setBusy(`running ${name}…`);
        refreshFeed();
        return impl(kwargs);
      };
    }

    try {
      const agent = loaded.sdk.BrowserAgent.start(loaded.wasm, {
        source: buildHarnessSource(example.code, input),
        llm: async (payload: { text: string; opts?: unknown }) => {
          if (stale()) return hang();
          setBusy(getOpenRouterKey() ? 'calling the model via OpenRouter…' : 'answering with the offline test reply…');
          refreshFeed();
          return decidePrompt(payload);
        },
        tools,
        fetchImpl: fetchWithSimulatedFallback as typeof fetch,
        onInput: (payload: { prompt: string; opts?: Json }) => {
          if (stale()) return hang();
          setBusy(null);
          refreshFeed();
          const opts = (payload.opts ?? {}) as Record<string, Json>;
          setAnswerDraft(String(opts.default ?? ''));
          return new Promise<string>((resolve) => {
            setPending({ prompt: payload.prompt, opts, resolve });
          });
        },
      }) as AgentHandle;
      agentRef.current = agent;
      setPhase('running');
      setBusy('running…');
      const result = await agent.run();
      if (stale()) return;
      setBusy(null);
      setPending(null);
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
      const loaded = await loadAssets();
      const replayed = loaded.sdk.BrowserAgent.restore(loaded.wasm, agent.blob(), {
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
      className="fixed inset-y-0 right-0 z-50 flex w-full flex-col border-l border-fd-border bg-fd-background shadow-2xl sm:w-[26rem]"
    >
      <div className="flex items-center gap-2 border-b border-fd-border px-3 py-2.5">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" aria-hidden>
          <path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z" />
        </svg>
        <div className="min-w-0">
          <p className="truncate text-sm font-medium">Run this example</p>
          <p className="truncate text-[11px] text-fd-muted-foreground">
            {example.title} · live on the wasm engine in this tab
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
              One login runs every docs example (and the playground). Until then, prompts return a
              deterministic offline reply.
            </span>
          </>
        )}
      </div>

      <div ref={feedBoxRef} className="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-contain p-3">
        <details className="rounded-lg border border-fd-border bg-fd-card/50">
          <summary className="cursor-pointer select-none px-3 py-2 text-xs font-medium">
            The code this runs{example.mode === 'fragment' ? ' (wrapped in an async run for you)' : ''}
          </summary>
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
            byte-identically, with zero live calls.
          </p>
        )}

        {feed.map((event, i) => (
          <EventCard key={i} event={event} />
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

      {statusLine && (
        <p
          id="runner-status"
          className="border-t border-fd-border bg-fd-accent/30 px-3 py-1.5 font-mono text-[11px] leading-relaxed text-fd-muted-foreground"
        >
          {statusLine}
        </p>
      )}
      <div className="flex items-center gap-1.5 border-t border-fd-border p-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))]">
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
          disabled={running || (feed.length === 0 && !statusLine)}
          onClick={reset}
        >
          Clear
        </button>
      </div>
    </aside>
  );
}
