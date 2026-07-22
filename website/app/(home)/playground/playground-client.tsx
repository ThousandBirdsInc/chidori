'use client';

/**
 * The /playground chat: a chidori agent (agent-source.ts) running on the wasm
 * engine in this tab, talking through a ReAct loop. This component is the
 * *host*: it serves `chidori.prompt()` from a brain (offline router or
 * OpenRouter), `chidori.tool()` from tools.ts, and `chidori.input()` from the
 * chat box. The feed renders purely from the journaled console, so restored
 * and replayed runs repaint identically — cards included.
 *
 * The wasm engine + browser SDK load at runtime from /public (build artifacts
 * of scripts/build-wasm.sh, hence the webpackIgnore'd imports); the docs
 * index loads from /playground-docs.json (scripts/build-playground-context.mjs).
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { AGENT_SOURCE } from './agent-source';
import {
  type ChatMessage,
  type DocsIndex,
  type FeedEvent,
  type Json,
  mockDecide,
  openRouterDecide,
  parseFeed,
  prepareDocsIndex,
} from './brain';
import { makeTools } from './tools';
import { ToolCard } from './cards';

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
const ASSETS = `${BASE}/chidori-wasm`;
const SAVE_KEY = 'chidori-playground-chat-v1';
// The exchanged OpenRouter key lives in sessionStorage: it survives the PKCE
// redirect back to this page, and is gone when the tab closes.
const OR_KEY = 'chidori-playground-openrouter-key';

const SUGGESTIONS = [
  'What is chidori, in one paragraph?',
  'How does offline replay work?',
  'Weather in Tokyo',
  'Chart the first 10 fibonacci numbers',
  'What is 2^16 / 3?',
  'Roll 3d6',
  'A color palette for a storm at dusk',
];

interface RunView {
  status: string;
  console: string[];
  liveCalls: number;
}

interface AgentHandle {
  run(): Promise<RunView>;
  console(): string[];
  blob(): Uint8Array;
}

declare global {
  interface Window {
    /** Set once the wasm assets are loaded; smoke checks key off this. */
    __chidoriReady?: boolean;
  }
}

interface Loaded {
  wasm: unknown;
  sdk: typeof import('../../../../sdk/browser/index.js');
}

async function loadAssets(): Promise<Loaded> {
  // Runtime imports on purpose: the wasm module and SDK are static assets,
  // not bundle modules. webpackIgnore keeps Next from trying to resolve them
  // at build time (they may not be built yet when only editing prose).
  const wasm = await import(
    /* webpackIgnore: true */ `${ASSETS}/chidori_wasm.js`
  );
  await wasm.default();
  const sdk = await import(
    /* webpackIgnore: true */ `${ASSETS}/chidori-browser.js`
  );
  return { wasm, sdk };
}

export function PlaygroundClient() {
  const [ready, setReady] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [feed, setFeed] = useState<FeedEvent[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [statusLine, setStatusLine] = useState('');
  const [hasSaved, setHasSaved] = useState(false);
  const [draft, setDraft] = useState('');
  const [provider, setProvider] = useState<'mock' | 'openrouter'>('mock');
  const [orKey, setOrKey] = useState<string | null>(null);
  const [model, setModel] = useState('openrouter/auto');

  const loadedRef = useRef<Loaded | null>(null);
  const loadedPromiseRef = useRef<Promise<Loaded | null> | null>(null);
  const agentRef = useRef<AgentHandle | null>(null);
  // The host closures live as long as the agent; anything they must see live
  // goes through a ref. `token` invalidates a whole agent on Reset.
  const tokenRef = useRef(0);
  const resolveRef = useRef<((answer: string) => void) | null>(null);
  const queueRef = useRef<string[]>([]);
  const providerRef = useRef(provider);
  const orKeyRef = useRef(orKey);
  const modelRef = useRef(model);
  const docsIndexRef = useRef<DocsIndex | null>(null);
  const feedBoxRef = useRef<HTMLDivElement | null>(null);

  const refreshFeed = useCallback(() => {
    const agent = agentRef.current;
    if (agent) setFeed(parseFeed(agent.console()));
  }, []);

  const persist = useCallback(() => {
    const agent = agentRef.current;
    if (!agent || agent.console().length === 0) return;
    try {
      localStorage.setItem(SAVE_KEY, new TextDecoder().decode(agent.blob()));
      setHasSaved(true);
    } catch {
      /* storage full/blocked — chat still works, just not durable */
    }
  }, []);

  /** Host implementations for one agent generation. */
  const buildHost = useCallback(
    (token: number) => {
      const stale = () => token !== tokenRef.current;
      const hang = () => new Promise<never>(() => {});
      const baseTools = makeTools(() => docsIndexRef.current);
      const tools: Record<string, (kwargs: Json) => Promise<Json>> = {};
      for (const [name, impl] of Object.entries(baseTools)) {
        tools[name] = async (kwargs) => {
          if (stale()) return hang();
          setBusy(`running ${name}…`);
          refreshFeed();
          return impl(kwargs);
        };
      }
      return {
        llm: async ({ text }: { text: string }) => {
          if (stale()) return hang();
          setBusy('thinking…');
          refreshFeed();
          let transcript: ChatMessage[] = [];
          try {
            transcript = JSON.parse(text);
          } catch {
            /* not a transcript — leave empty */
          }
          try {
            const decision =
              providerRef.current === 'openrouter' && orKeyRef.current
                ? await openRouterDecide({
                    apiKey: orKeyRef.current,
                    model: modelRef.current,
                    transcript,
                    index: docsIndexRef.current,
                  })
                : mockDecide(transcript, docsIndexRef.current);
            return JSON.stringify(decision);
          } catch (err) {
            // Keep the loop alive: surface the failure as the reply.
            return JSON.stringify({ reply: `LLM call failed: ${String(err)}` });
          }
        },
        tools,
        onInput: () => {
          if (stale()) return hang();
          setBusy(null);
          refreshFeed();
          persist();
          const queued = queueRef.current.shift();
          if (queued !== undefined) return queued;
          return new Promise<string>((resolve) => {
            resolveRef.current = resolve;
          });
        },
      };
    },
    [refreshFeed, persist],
  );

  const drive = useCallback(
    (agent: AgentHandle, token: number) => {
      agent
        .run()
        .then(() => {
          if (token !== tokenRef.current) return;
          setBusy(null);
          refreshFeed();
        })
        .catch((err) => {
          if (token !== tokenRef.current) return;
          setBusy(null);
          setStatusLine(`Agent error: ${String(err)}`);
        });
    },
    [refreshFeed],
  );

  /** Restore a saved conversation: replays the journal, waits at input. */
  const restoreSaved = useCallback(
    (loaded: Loaded): boolean => {
      const saved = localStorage.getItem(SAVE_KEY);
      if (saved === null) return false;
      try {
        const token = tokenRef.current;
        const agent = loaded.sdk.BrowserAgent.restore(
          loaded.wasm,
          new TextEncoder().encode(saved),
          buildHost(token),
        ) as AgentHandle;
        agentRef.current = agent;
        setHasSaved(true);
        setStatusLine('Restored from localStorage — earlier turns replayed offline.');
        drive(agent, token);
        return true;
      } catch {
        localStorage.removeItem(SAVE_KEY);
        return false;
      }
    },
    [buildHost, drive],
  );

  useEffect(() => {
    let cancelled = false;
    const loading = loadAssets();
    loadedPromiseRef.current = loading.catch(() => null);
    fetch(`${BASE}/playground-docs.json`)
      .then((res) => (res.ok ? res.json() : null))
      .then((json) => {
        if (json && !cancelled) docsIndexRef.current = prepareDocsIndex(json);
      })
      .catch(() => {
        /* docs search degrades gracefully */
      });
    loading
      .then(async (loaded) => {
        if (cancelled) return;
        loadedRef.current = loaded;
        // Finish the OpenRouter PKCE login if this page load is the redirect
        // back from openrouter.ai (no-op otherwise).
        try {
          const exchanged = await loaded.sdk.completeOpenRouterLogin();
          if (exchanged) sessionStorage.setItem(OR_KEY, exchanged);
        } catch (err) {
          setStatusLine(`OpenRouter login failed: ${String(err)}`);
        }
        const key = sessionStorage.getItem(OR_KEY);
        if (key) {
          orKeyRef.current = key;
          setOrKey(key);
          providerRef.current = 'openrouter';
          setProvider('openrouter');
        }
        setReady(true);
        window.__chidoriReady = true;
        restoreSaved(loaded);
      })
      .catch(() => {
        if (!cancelled) {
          setLoadError(
            'The wasm assets are missing. Build them with scripts/build-wasm.sh, then reload.',
          );
        }
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const box = feedBoxRef.current;
    if (box) box.scrollTop = box.scrollHeight;
  }, [feed, busy]);

  const ensureAgent = useCallback(async () => {
    if (agentRef.current) return;
    const loaded = loadedRef.current ?? (await loadedPromiseRef.current);
    if (!loaded || agentRef.current) return;
    const token = tokenRef.current;
    const agent = loaded.sdk.BrowserAgent.start(loaded.wasm, {
      source: AGENT_SOURCE,
      ...buildHost(token),
    }) as AgentHandle;
    agentRef.current = agent;
    drive(agent, token);
  }, [buildHost, drive]);

  const send = useCallback(
    (text: string) => {
      const message = text.trim();
      if (!message || !ready) return;
      setDraft('');
      setStatusLine('');
      const resolve = resolveRef.current;
      if (resolve) {
        resolveRef.current = null;
        resolve(message);
      } else {
        queueRef.current.push(message);
        void ensureAgent();
      }
    },
    [ready, ensureAgent],
  );

  const replay = useCallback(async () => {
    const loaded = loadedRef.current;
    const saved = localStorage.getItem(SAVE_KEY);
    if (!loaded || saved === null) return;
    try {
      // No brain, no tools, no network: every effect must come from the
      // journal. The whole chat — cards included — repaints from it.
      const agent = loaded.sdk.BrowserAgent.restore(
        loaded.wasm,
        new TextEncoder().encode(saved),
        {
          llm: () => {
            throw new Error('replay must not call the LLM');
          },
          fetchImpl: (() => {
            throw new Error('replay must not touch the network');
          }) as unknown as typeof fetch,
        },
      ) as AgentHandle;
      const result = await agent.run();
      setFeed(parseFeed(result.console));
      setStatusLine(
        `⚡ Replayed offline: ${result.console.length} journaled events re-rendered with ${result.liveCalls} live host calls.`,
      );
    } catch (err) {
      setStatusLine(`Replay error: ${String(err)}`);
    }
  }, []);

  const reset = useCallback(() => {
    tokenRef.current += 1; // orphan the running agent; its host calls hang
    agentRef.current = null;
    resolveRef.current = null;
    queueRef.current = [];
    localStorage.removeItem(SAVE_KEY);
    setFeed([]);
    setBusy(null);
    setStatusLine('');
    setHasSaved(false);
  }, []);

  const connectOpenRouter = useCallback(() => {
    // Redirects to openrouter.ai's consent page; the redirect back lands on
    // this page with ?code=, which the mount effect exchanges for a key.
    void loadedRef.current?.sdk.startOpenRouterLogin();
  }, []);

  const disconnectOpenRouter = useCallback(() => {
    sessionStorage.removeItem(OR_KEY);
    orKeyRef.current = null;
    setOrKey(null);
    providerRef.current = 'mock';
    setProvider('mock');
  }, []);

  const pickProvider = useCallback((p: 'mock' | 'openrouter') => {
    providerRef.current = p;
    setProvider(p);
  }, []);

  if (loadError) {
    return (
      <div className="mt-8 rounded-lg border border-fd-border bg-fd-card p-6 text-fd-muted-foreground">
        {loadError}
      </div>
    );
  }

  const button =
    'rounded-lg border border-fd-border px-3 py-1.5 text-sm font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';
  const segment = (active: boolean) =>
    `rounded-md px-2.5 py-1 text-sm font-medium transition-colors ${
      active ? 'bg-fd-background shadow-sm' : 'text-fd-muted-foreground hover:text-fd-foreground'
    }`;

  return (
    <div className="mt-6">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 text-sm">
        <div className="flex items-center gap-1 rounded-lg bg-fd-accent/60 p-1" role="group" aria-label="Brain">
          <button id="provider-mock" className={segment(provider === 'mock')} onClick={() => pickProvider('mock')}>
            Offline brain
          </button>
          <button
            id="provider-openrouter"
            className={segment(provider === 'openrouter')}
            onClick={() => pickProvider('openrouter')}
          >
            OpenRouter
          </button>
        </div>
        {provider === 'openrouter' &&
          (orKey ? (
            <span className="flex items-center gap-2">
              <span id="or-connected" className="text-fd-muted-foreground">
                ✓ connected
              </span>
              <input
                id="or-model"
                type="text"
                value={model}
                onChange={(e) => {
                  modelRef.current = e.target.value;
                  setModel(e.target.value);
                }}
                aria-label="OpenRouter model"
                className="w-52 rounded-lg border border-fd-border bg-fd-background px-2 py-1"
              />
              <button id="or-disconnect" className={button} onClick={disconnectOpenRouter}>
                Disconnect
              </button>
            </span>
          ) : (
            <button id="or-connect" className={button} onClick={connectOpenRouter} disabled={!ready}>
              Connect OpenRouter
            </button>
          ))}
        <span className="ml-auto flex gap-2">
          <button id="replay" className={button} disabled={!ready || !hasSaved} onClick={replay} title="Re-render this chat from its journal — zero live calls">
            ↺ Replay offline
          </button>
          <button id="clear" className={button} disabled={!hasSaved && feed.length === 0} onClick={reset}>
            ✕ Reset
          </button>
        </span>
      </div>
      {provider === 'openrouter' && !orKey && (
        <p className="mt-2 text-xs text-fd-muted-foreground">
          Not connected yet — messages fall back to the offline brain until you connect.
        </p>
      )}

      <div className="mt-3 overflow-hidden rounded-xl border border-fd-border bg-fd-card/50">
        <div ref={feedBoxRef} className="h-[28rem] overflow-y-auto p-4">
          {feed.length === 0 && !busy ? (
            <div className="flex h-full flex-col items-center justify-center gap-4 text-center">
              <p className="text-sm text-fd-muted-foreground">
                {ready ? 'Ask about chidori, or put the tools to work:' : 'Loading the wasm engine…'}
              </p>
              <div className="flex max-w-lg flex-wrap justify-center gap-2">
                {SUGGESTIONS.map((s) => (
                  <button
                    key={s}
                    className="rounded-full border border-fd-border px-3 py-1.5 text-xs transition-colors hover:bg-fd-accent disabled:opacity-40"
                    disabled={!ready}
                    onClick={() => send(s)}
                  >
                    {s}
                  </button>
                ))}
              </div>
            </div>
          ) : (
            <div className="flex flex-col gap-3">
              {feed.map((event, i) => {
                if (event.kind === 'user') {
                  return (
                    <div key={i} className="flex justify-end">
                      <p className="max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-md bg-fd-primary px-3.5 py-2 text-sm text-fd-primary-foreground">
                        {event.text}
                      </p>
                    </div>
                  );
                }
                if (event.kind === 'assistant') {
                  return (
                    <div key={i} className="flex">
                      <p className="max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-bl-md border border-fd-border bg-fd-background px-3.5 py-2 text-sm">
                        {event.text}
                      </p>
                    </div>
                  );
                }
                if (event.kind === 'tool') {
                  return (
                    <div key={i} className="flex">
                      <ToolCard name={event.name} args={event.args} result={event.result} />
                    </div>
                  );
                }
                return (
                  <p key={i} className="text-center text-xs text-fd-muted-foreground">
                    {event.text}
                  </p>
                );
              })}
              {busy && (
                <p className="animate-pulse text-xs text-fd-muted-foreground" id="busy">
                  {busy}
                </p>
              )}
            </div>
          )}
        </div>
        <form
          className="flex gap-2 border-t border-fd-border p-3"
          onSubmit={(e) => {
            e.preventDefault();
            send(draft);
          }}
        >
          <input
            id="chat-input"
            type="text"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            placeholder={ready ? 'Message the agent…' : 'Loading…'}
            disabled={!ready}
            autoComplete="off"
            className="min-w-0 flex-1 rounded-lg border border-fd-border bg-fd-background px-3 py-2 text-sm outline-none focus:ring-2 focus:ring-fd-primary/40"
          />
          <button id="send" type="submit" className={button} disabled={!ready || !draft.trim()}>
            Send
          </button>
        </form>
      </div>

      {statusLine && (
        <p id="status" className="mt-2 text-sm text-fd-muted-foreground">
          {statusLine}
        </p>
      )}

      <details className="mt-8 rounded-lg border border-fd-border p-4">
        <summary className="cursor-pointer text-sm font-medium">Under the hood</summary>
        <ul className="mt-3 list-disc space-y-1 pl-5 text-sm text-fd-muted-foreground">
          <li>
            This chat is a chidori agent — the source below — executed by the pure-Rust engine
            compiled to WebAssembly, entirely in this tab.
          </li>
          <li>
            Every <code>chidori.prompt / tool / input</code> effect is journaled: the conversation
            auto-saves each turn, survives a reload, and <em>Replay offline</em> repaints it — cards
            and all — with zero live calls.
          </li>
          <li>
            Docs answers are grounded: these docs are indexed at build time, retrieved into the
            model&apos;s context, and exposed as the <code>search_docs</code> tool.
          </li>
        </ul>
        <pre className="mt-3 overflow-x-auto rounded-lg border border-fd-border bg-fd-card p-4 text-xs">
          {AGENT_SOURCE}
        </pre>
      </details>
    </div>
  );
}
