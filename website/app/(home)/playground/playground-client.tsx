'use client';

/**
 * The /playground chat: a chidori agent (agent-source.ts) running on the wasm
 * engine in this tab, talking through a ReAct loop. This component is the
 * *host*: it serves `chidori.prompt()` from a brain (offline router or
 * OpenRouter), `chidori.tool()` from tools.ts, and `chidori.input()` from the
 * chat box. The feed renders purely from the journaled console, so restored
 * and replayed runs repaint identically — cards included.
 *
 * The agent's source is itself mutable *from inside the chat*: the
 * update_source tool stages a replacement, validated by replaying the live
 * journal against the new bundle, and when the turn ends the page hot-swaps
 * the code in (modify-and-resume: same journal, new program).
 *
 * The wasm engine + browser SDK load at runtime from /public (build artifacts
 * of scripts/build-wasm.sh, hence the webpackIgnore'd imports); the docs
 * index loads from /playground-docs.json (scripts/build-playground-context.mjs).
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import dynamic from 'next/dynamic';
import { OPENROUTER_KEY_STORAGE } from '@/lib/openrouter';
import { DEFAULT_AGENT_SOURCE } from './agent-source';
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
import { ToolCard, FormResponseBubble } from './cards';
import { formResponseMessage, pairFormResponses, type FormData } from './form-schema';
import {
  type BranchStore,
  countTurns,
  freshBranches,
  truncateAtTurn,
} from './timeline';
import {
  freshHistory,
  moveHead,
  parseHistory,
  recordCommit,
  type PlaygroundCommit,
  type SourceHistoryEvent,
  type SourceHistoryStore,
} from './source-history';
import { HistoryPanel } from './history-panel';

// The CodeMirror editor is a heavy chunk; load it only when the
// "under the hood" panel is first opened.
const SourceEditor = dynamic(
  () => import('./source-editor').then((m) => m.SourceEditor),
  {
    ssr: false,
    loading: () => <p className="mt-2 text-xs text-fd-muted-foreground">Loading editor…</p>,
  },
);

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
const ASSETS = `${BASE}/chidori-wasm`;
const SAVE_KEY = 'chidori-playground-chat-v1';
const BRANCH_KEY = 'chidori-playground-branches-v1';
const SOURCE_KEY = 'chidori-playground-source-v1';
const HISTORY_KEY = 'chidori-playground-history-v1';
// The exchanged OpenRouter key lives in sessionStorage: it survives the PKCE
// redirect back to this page, and is gone when the tab closes. The storage
// key is shared site-wide (lib/openrouter.ts), so one login also powers the
// docs pages' runnable examples — and a login started there works here.
const OR_KEY = OPENROUTER_KEY_STORAGE;

interface Example {
  prompt: string;
  title: string;
  /** What the example actually demonstrates about the runtime. */
  detail: string;
}

const EXAMPLES: Example[] = [
  {
    prompt: 'What is chidori, in one paragraph?',
    title: 'Ask what chidori is',
    detail:
      'Docs-grounded Q&A: the agent retrieves from these docs with its search_docs tool, so the answer is cited, not guessed.',
  },
  {
    prompt: 'How does offline replay work?',
    title: 'Ask about offline replay',
    detail:
      'Another retrieval-grounded answer — this one explains the effect journal that makes this very chat replayable.',
  },
  {
    prompt: 'Show me your own source code',
    title: 'Read its own source',
    detail:
      'The read_source tool returns agent.ts — the live program running this chat, the same code shown under the hood below.',
  },
  {
    prompt: 'Rewrite your code: add a ⚡ to every reply',
    title: 'Rewrite itself mid-chat',
    detail:
      'Self-modification: update_source stages new code, validates it by replaying the journal, then hot-swaps it in — same conversation, new program.',
  },
  {
    prompt: 'Weather in Tokyo',
    title: 'Call a live tool',
    detail:
      'A real network call runs exactly once, its result is journaled, and every replay repaints the weather card offline.',
  },
  {
    prompt: 'Make a form to plan a trip',
    title: 'Generate an inline form',
    detail:
      'Generative UI: the form tool emits a JSON Schema the page renders, and submitting flows through the same journaled chidori.input() as a typed message.',
  },
  {
    prompt: 'Chart the first 10 fibonacci numbers',
    title: 'Draw a chart',
    detail:
      'The model computes the series, and the chart card re-renders from the journaled tool result on every restore and replay.',
  },
  {
    prompt: 'Roll 3d6',
    title: 'Roll some dice',
    detail:
      'Journaled randomness: the rolls are recorded as effects, so replays repaint the same dice instead of re-rolling.',
  },
  {
    prompt: 'A color palette for a storm at dusk',
    title: 'Paint a color palette',
    detail:
      'A mood becomes deterministic swatches, rendered as a card straight from the journaled tool result.',
  },
];

/**
 * The empty chat's example picker: a dropdown instead of a wall of pills,
 * so each example has room to say what it actually demonstrates. Picking
 * one sends its prompt as the first message.
 */
function ExamplePicker({
  disabled,
  onPick,
}: {
  disabled: boolean;
  onPick: (prompt: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const listRef = useRef<HTMLUListElement | null>(null);

  useEffect(() => {
    if (!open) return;
    // The picker sits centered in the scrollable feed box; the panel opens
    // downward past the fold, so bring it fully into view.
    listRef.current?.scrollIntoView({ block: 'nearest' });
    const close = (e: PointerEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('pointerdown', close);
    return () => document.removeEventListener('pointerdown', close);
  }, [open]);

  return (
    <div ref={boxRef} className="relative">
      <button
        id="example-picker"
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        className="inline-flex h-9 items-center gap-2 rounded-lg border border-fd-border bg-fd-background px-3.5 text-sm font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40"
        onClick={() => setOpen((o) => !o)}
      >
        Try an example
        <svg
          width="10"
          height="10"
          viewBox="0 0 10 10"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          aria-hidden
          className={`transition-transform ${open ? 'rotate-180' : ''}`}
        >
          <path d="m2 3.5 3 3 3-3" />
        </svg>
      </button>
      {open && (
        <ul
          ref={listRef}
          role="listbox"
          aria-label="Example prompts"
          className="absolute left-1/2 top-full z-10 mt-2 max-h-72 w-[min(24rem,calc(100vw-3rem))] -translate-x-1/2 overflow-y-auto rounded-xl border border-fd-border bg-fd-background p-1.5 text-left shadow-lg"
        >
          {EXAMPLES.map((ex) => (
            <li key={ex.prompt} role="option" aria-selected={false}>
              <button
                type="button"
                className="w-full rounded-lg px-3 py-2 text-left transition-colors hover:bg-fd-accent"
                onClick={() => {
                  setOpen(false);
                  onPick(ex.prompt);
                }}
              >
                <span className="block text-sm font-medium">{ex.title}</span>
                <span className="mt-0.5 block font-mono text-[11px] text-fd-primary">
                  “{ex.prompt}”
                </span>
                <span className="mt-0.5 block text-xs leading-relaxed text-fd-muted-foreground">
                  {ex.detail}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

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

/** The slice of the wasm module's surface the hot-swap machinery touches. */
interface WasmModule {
  stripTypes(source: string, filename: string): string;
  WasmRuntime: {
    fromBlob(bytes: Uint8Array): {
      runUntilBlocked(): string;
      divergence(): string | undefined;
      free?: () => void;
    };
  };
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
  const [branchState, setBranchState] = useState<BranchStore>(freshBranches);
  const [source, setSource] = useState(DEFAULT_AGENT_SOURCE);
  const [history, setHistory] = useState<SourceHistoryStore>(freshHistory);
  /** Latches true the first time "under the hood" opens (mounts the editor). */
  const [hoodOpened, setHoodOpened] = useState(false);

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
  /** Follow new feed events only while the reader is near the bottom. */
  const pinnedRef = useRef(true);
  const branchRef = useRef(branchState);
  const sourceRef = useRef(source);
  const historyRef = useRef(history);
  /** A history restore in flight: its commit id tags the resulting commit. */
  const pendingRestoreRef = useRef<string | null>(null);
  /** An update_source edit accepted this turn, waiting for the turn to end. */
  const pendingSourceRef = useRef<string | null>(null);
  /** Reverts an in-flight hot-swap if replaying the full journal fails. */
  const swapFallbackRef = useRef<((err: string) => void) | null>(null);
  const hotSwapRef = useRef<((next: string) => void) | null>(null);

  const updateBranches = useCallback((next: BranchStore) => {
    branchRef.current = next;
    setBranchState(next);
    try {
      localStorage.setItem(BRANCH_KEY, JSON.stringify(next));
    } catch {
      /* storage full/blocked — branches just won't survive a reload */
    }
  }, []);

  const updateHistory = useCallback((next: SourceHistoryStore) => {
    historyRef.current = next;
    setHistory(next);
    try {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(next));
    } catch {
      /* storage full/blocked — history just won't survive a reload */
    }
  }, []);

  /**
   * Record the source version now active into the implementation history —
   * the playground's mirror of the runtime's source history
   * (docs/source-history.md). Recording dedupes against HEAD, so every call
   * site invokes it unconditionally; `turnFrontier` is the number of user
   * turns already journaled, anchoring the version to the execution history
   * exactly like a run commit's journal frontier. Async (WebCrypto hashing)
   * and best-effort — callers fire and forget.
   */
  const recordHistory = useCallback(
    (event: SourceHistoryEvent, text: string, restores?: string) => {
      const agent = agentRef.current;
      const blobText = agent
        ? new TextDecoder().decode(agent.blob())
        : localStorage.getItem(SAVE_KEY) ?? '';
      const frontier = blobText ? countTurns(blobText) : 0;
      void recordCommit(historyRef.current, event, text, frontier, { restores })
        .then((result) => {
          if (result) updateHistory(result.store);
        })
        .catch(() => {
          /* hashing unavailable — history stays as it was */
        });
    },
    [updateHistory],
  );

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

  /** Make `next` the displayed + persisted agent source. */
  const applySource = useCallback((next: string) => {
    sourceRef.current = next;
    setSource(next);
    try {
      if (next === DEFAULT_AGENT_SOURCE) localStorage.removeItem(SOURCE_KEY);
      else localStorage.setItem(SOURCE_KEY, next);
    } catch {
      /* storage full/blocked — the swap still works, just not durable */
    }
  }, []);

  /**
   * Prove the conversation can move onto `next`: compile it, then restore a
   * scratch runtime from the live blob with the bundle swapped and pump it to
   * the journal frontier. An edit that changes any already-executed effect
   * call fails replay (divergence) and throws here — inside the tool call —
   * so the model reads the reason and can try a smaller patch.
   */
  const validateSwap = useCallback((next: string) => {
    const loaded = loadedRef.current;
    if (!loaded) throw new Error('engine not loaded yet');
    const wasm = loaded.wasm as WasmModule;
    const bundle = loaded.sdk.PRELUDE + wasm.stripTypes(next, 'agent.ts');
    const agent = agentRef.current;
    if (!agent) return; // no history yet — nothing to replay against
    const blob = JSON.parse(new TextDecoder().decode(agent.blob())) as { bundle: string };
    blob.bundle = bundle;
    const scratch = wasm.WasmRuntime.fromBlob(new TextEncoder().encode(JSON.stringify(blob)));
    try {
      // Replayed entries resolve internally; one pump reaches the frontier.
      // Divergence detected mid-pump unwinds the program to a quiet
      // completion, so ask for it explicitly rather than relying on a throw.
      const status = JSON.parse(scratch.runUntilBlocked()) as { status: string };
      const diverged = scratch.divergence();
      if (diverged) {
        throw new Error(`the edit changes code that already ran, so the journal cannot replay: ${diverged}`);
      }
      if (status.status === 'completed') {
        throw new Error(
          'the new source ran to completion instead of waiting at chidori.input() — the chat would end',
        );
      }
    } finally {
      scratch.free?.();
    }
  }, []);

  /** Validate `next` and stage it; the swap happens when the turn ends. */
  const proposeSource = useCallback(
    (next: string) => {
      validateSwap(next);
      pendingSourceRef.current = next;
    },
    [validateSwap],
  );

  /** Host implementations for one agent generation. */
  const buildHost = useCallback(
    (token: number) => {
      const stale = () => token !== tokenRef.current;
      const hang = () => new Promise<never>(() => {});
      const baseTools = makeTools(() => docsIndexRef.current, {
        getSource: () => pendingSourceRef.current ?? sourceRef.current,
        defaultSource: DEFAULT_AGENT_SOURCE,
        propose: proposeSource,
      });
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
          // Reaching input means the journal replayed cleanly — a hot-swap
          // in flight has succeeded, so disarm its revert.
          swapFallbackRef.current = null;
          const staged = pendingSourceRef.current;
          if (staged !== null) {
            // The turn that staged an edit just ended: swap now. This agent
            // is orphaned (its input hangs); the new one restores from the
            // same journal with the new bundle and waits at input instead.
            pendingSourceRef.current = null;
            setTimeout(() => hotSwapRef.current?.(staged), 0);
            return hang();
          }
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
    [refreshFeed, persist, proposeSource],
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
          const fallback = swapFallbackRef.current;
          if (fallback) {
            // A hot-swapped journal failed to replay: put the old code back.
            swapFallbackRef.current = null;
            fallback(String(err));
            return;
          }
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
        // Conversations saved before history existed get their root commit
        // here; dedupe makes this a no-op for every later restore.
        recordHistory(
          historyRef.current.commits.length === 0 ? 'run_start' : 'source_change',
          sourceRef.current,
        );
        return true;
      } catch {
        localStorage.removeItem(SAVE_KEY);
        return false;
      }
    },
    [buildHost, drive, recordHistory],
  );

  useEffect(() => {
    let cancelled = false;
    try {
      const raw = localStorage.getItem(BRANCH_KEY);
      if (raw) {
        const parsed = JSON.parse(raw) as BranchStore;
        if (parsed && typeof parsed.activeLabel === 'string' && Array.isArray(parsed.stashed)) {
          branchRef.current = parsed;
          setBranchState(parsed);
        }
      }
    } catch {
      /* corrupted branch store — start fresh */
    }
    try {
      const rawHistory = localStorage.getItem(HISTORY_KEY);
      const parsedHistory = rawHistory ? parseHistory(rawHistory) : null;
      if (parsedHistory) {
        historyRef.current = parsedHistory;
        setHistory(parsedHistory);
      }
    } catch {
      /* storage blocked — history starts fresh */
    }
    try {
      const savedSource = localStorage.getItem(SOURCE_KEY);
      if (savedSource !== null) {
        sourceRef.current = savedSource;
        setSource(savedSource);
      }
    } catch {
      /* storage blocked — run the default source */
    }
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
    if (box && pinnedRef.current) box.scrollTop = box.scrollHeight;
  }, [feed, busy]);

  /** Scrolling up unpins the feed so new events don't yank the view down. */
  const onFeedScroll = useCallback(() => {
    const box = feedBoxRef.current;
    if (!box) return;
    pinnedRef.current = box.scrollHeight - box.scrollTop - box.clientHeight < 96;
  }, []);

  const ensureAgent = useCallback(async () => {
    if (agentRef.current) return;
    const loaded = loadedRef.current ?? (await loadedPromiseRef.current);
    if (!loaded || agentRef.current) return;
    const token = tokenRef.current;
    const agent = loaded.sdk.BrowserAgent.start(loaded.wasm, {
      source: sourceRef.current,
      ...buildHost(token),
    }) as AgentHandle;
    agentRef.current = agent;
    drive(agent, token);
    // The version that starts the run is the history's root — the same
    // `run_start` commit the runtime records on a run's first persist.
    recordHistory(
      historyRef.current.commits.length === 0 ? 'run_start' : 'source_change',
      sourceRef.current,
    );
  }, [buildHost, drive, recordHistory]);

  const send = useCallback(
    (text: string) => {
      const message = text.trim();
      if (!message || !ready) return;
      setDraft('');
      setStatusLine('');
      pinnedRef.current = true;
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

  /**
   * A form card's submit: the answers travel the exact same path as a typed
   * message, so the agent receives them as one journaled `chidori.input()` —
   * the runtime never knows a form was involved.
   */
  const submitForm = useCallback(
    (id: string, values: FormData) => {
      send(formResponseMessage(id, values));
    },
    [send],
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

  /** The active timeline's durable blob, from the live agent or the save. */
  const currentBlobText = useCallback((): string | null => {
    const agent = agentRef.current;
    if (agent) return new TextDecoder().decode(agent.blob());
    return localStorage.getItem(SAVE_KEY);
  }, []);

  /** Orphan the running agent and restore a fresh one from `blobText`. */
  const startFromBlob = useCallback(
    (blobText: string, note: string): boolean => {
      const loaded = loadedRef.current;
      if (!loaded) return false;
      tokenRef.current += 1; // orphan the old agent; its host calls hang
      const token = tokenRef.current;
      agentRef.current = null;
      resolveRef.current = null;
      queueRef.current = [];
      setBusy(null);
      try {
        const agent = loaded.sdk.BrowserAgent.restore(
          loaded.wasm,
          new TextEncoder().encode(blobText),
          buildHost(token),
        ) as AgentHandle;
        agentRef.current = agent;
        localStorage.setItem(SAVE_KEY, blobText);
        setHasSaved(true);
        // The feed repaints from the replayed console as soon as the run
        // reaches its input frontier (onInput → refreshFeed).
        setStatusLine(note);
        drive(agent, token);
        return true;
      } catch (err) {
        setStatusLine(`Restore failed: ${String(err)}`);
        return false;
      }
    },
    [buildHost, drive],
  );

  /**
   * Move the conversation onto `next`: swap the durable blob's bundle for the
   * newly compiled source and restore — the whole journal replays against the
   * new code (modify-and-resume), so the chat keeps its history and even its
   * feed re-renders through the new implementation. `proposeSource` already
   * replay-validated up to the accepting tool call; the tail of that turn is
   * covered by the revert armed in swapFallbackRef.
   */
  const hotSwap = useCallback(
    (next: string) => {
      const loaded = loadedRef.current;
      const agent = agentRef.current;
      if (!loaded || !agent) return;
      const prevBlobText = new TextDecoder().decode(agent.blob());
      const prevSource = sourceRef.current;
      let nextBlobText: string;
      try {
        const wasm = loaded.wasm as WasmModule;
        const blob = JSON.parse(prevBlobText) as { bundle: string };
        blob.bundle = loaded.sdk.PRELUDE + wasm.stripTypes(next, 'agent.ts');
        nextBlobText = JSON.stringify(blob);
      } catch (err) {
        setStatusLine(`Hot-swap failed: ${String(err)}`);
        return;
      }
      // Unlike rewind/branch, a swap continues the same conversation — carry
      // any message the user typed while the swapping turn ran.
      const carried = [...queueRef.current];
      const deliver = () => {
        if (!carried.length) return;
        const resolve = resolveRef.current;
        if (resolve) {
          // The restored agent already replayed to its input and is waiting.
          resolveRef.current = null;
          queueRef.current.push(...carried.slice(1));
          resolve(carried[0]);
        } else {
          queueRef.current.push(...carried);
        }
      };
      applySource(next);
      swapFallbackRef.current = (err: string) => {
        applySource(prevSource);
        startFromBlob(
          prevBlobText,
          `⚠️ Hot-swap reverted — the journal could not replay under the new code: ${err}`,
        );
        // The revert is history too: the chain shows edit → back, and the
        // reverted commit reuses the original version's stored object
        // (content addressing — no second copy).
        recordHistory('source_change', prevSource);
        deliver();
      };
      const ok = startFromBlob(
        nextBlobText,
        next === DEFAULT_AGENT_SOURCE
          ? '🧬 Hot-swapped back to the original agent source — the journal replayed against it (modify-and-resume).'
          : '🧬 Hot-swapped the agent’s source mid-conversation: same journal, new code — every past turn just re-rendered through the new implementation.',
      );
      if (!ok) {
        const fallback = swapFallbackRef.current;
        swapFallbackRef.current = null;
        fallback?.('restore failed');
        return;
      }
      // Chain the accepted version onto the implementation history — a
      // `restore` when a history panel click swapped an old version back in,
      // an ordinary `source_change` otherwise.
      const restores = pendingRestoreRef.current;
      pendingRestoreRef.current = null;
      recordHistory(restores ? 'restore' : 'source_change', next, restores ?? undefined);
      deliver();
    },
    [applySource, startFromBlob, recordHistory],
  );

  useEffect(() => {
    hotSwapRef.current = hotSwap;
  }, [hotSwap]);

  /**
   * Manual edits from the source editor go through the same gate as the
   * chat's update_source tool — compile, replay-validate, hot-swap — they
   * just skip the "wait for the turn to end" step, because applying is only
   * enabled while the agent sits idle at `chidori.input()`.
   */
  const applyManualEdit = useCallback(
    (next: string): string | null => {
      const loaded = loadedRef.current;
      if (!loaded) return 'The engine is still loading.';
      if (!next.includes('chidori.input')) {
        return 'The source must keep awaiting chidori.input() in a loop, or the chat ends.';
      }
      try {
        if (!agentRef.current) {
          // Nothing recorded yet: compile-check now, run it on first message.
          (loaded.wasm as WasmModule).stripTypes(next, 'agent.ts');
          applySource(next);
          const restores = pendingRestoreRef.current;
          pendingRestoreRef.current = null;
          recordHistory(
            restores
              ? 'restore'
              : historyRef.current.commits.length === 0
                ? 'run_start'
                : 'source_change',
            next,
            restores ?? undefined,
          );
          setStatusLine('🧬 Source updated — your next message starts the agent on the edited code.');
          return null;
        }
        validateSwap(next);
        hotSwap(next);
        return null;
      } catch (err) {
        pendingRestoreRef.current = null;
        return String(err);
      }
    },
    [applySource, validateSwap, hotSwap, recordHistory],
  );

  /**
   * Rewind the active timeline to just before user turn `turn`: the journal
   * is truncated at that turn's `chidori.input()` entry and the shorter blob
   * restored — the surviving prefix replays offline and the agent waits at
   * input again, with the removed message queued up in the box for editing.
   */
  const rewindTo = useCallback(
    (turn: number, text: string) => {
      const current = currentBlobText();
      if (!current) return;
      const cut = truncateAtTurn(current, turn);
      if (cut === null) {
        setStatusLine('Rewind failed: that turn was not found in the journal.');
        return;
      }
      const dropped = countTurns(current) - turn;
      const note = `⟲ Rewound to before turn ${turn + 1}: the journal was truncated at that chidori.input() and replayed offline (${dropped} turn${dropped === 1 ? '' : 's'} discarded).`;
      if (startFromBlob(cut, note)) setDraft(text);
    },
    [currentBlobText, startFromBlob],
  );

  /**
   * Branch: stash the full-length blob as a switchable timeline, then rewind
   * this one. Nothing is lost — the discarded future lives on as a branch.
   */
  const branchFrom = useCallback(
    (turn: number, text: string) => {
      const current = currentBlobText();
      if (!current) return;
      const cut = truncateAtTurn(current, turn);
      if (cut === null) {
        setStatusLine('Branch failed: that turn was not found in the journal.');
        return;
      }
      const store = branchRef.current;
      const label = `path ${store.nextId}`;
      const note = `⑂ Stashed “${store.activeLabel}” as a branch and rewound to before turn ${turn + 1} — you are now on “${label}”.`;
      if (!startFromBlob(cut, note)) return;
      updateBranches({
        activeLabel: label,
        nextId: store.nextId + 1,
        stashed: [
          ...store.stashed,
          {
            label: store.activeLabel,
            blob: current,
            turns: countTurns(current),
            source: sourceRef.current,
            // Timelines are branch heads over the shared implementation
            // history: the stash remembers which commit its code sits at.
            headId: historyRef.current.head ?? undefined,
          },
        ],
      });
      setDraft(text);
    },
    [currentBlobText, startFromBlob, updateBranches],
  );

  /** Switch timelines: stash the active blob, restore the chosen one. */
  const switchTo = useCallback(
    (label: string) => {
      const store = branchRef.current;
      const target = store.stashed.find((b) => b.label === label);
      if (!target) return;
      const current = currentBlobText();
      const stashed = store.stashed.filter((b) => b.label !== label);
      if (current) {
        stashed.push({
          label: store.activeLabel,
          blob: current,
          turns: countTurns(current),
          source: sourceRef.current,
          headId: historyRef.current.head ?? undefined,
        });
      }
      if (
        startFromBlob(
          target.blob,
          `⑂ Switched to “${target.label}” — restored from its saved blob and replayed offline.`,
        )
      ) {
        // Timelines carry their own code: the blob's bundle is what runs, and
        // the branch's stashed source keeps the display honest.
        applySource(target.source ?? DEFAULT_AGENT_SOURCE);
        updateBranches({ activeLabel: target.label, nextId: store.nextId, stashed });
        if (target.headId) {
          // Switching timelines moves HEAD to that timeline's commit —
          // history itself never rewrites, exactly like `git switch`.
          updateHistory(moveHead(historyRef.current, target.headId));
        } else {
          // A stash from before history existed: give its code a commit so
          // HEAD matches what is actually running.
          recordHistory('source_change', target.source ?? DEFAULT_AGENT_SOURCE);
        }
      }
    },
    [currentBlobText, startFromBlob, updateBranches, applySource, updateHistory, recordHistory],
  );

  const dropBranch = useCallback(
    (label: string) => {
      const store = branchRef.current;
      updateBranches({
        ...store,
        stashed: store.stashed.filter((b) => b.label !== label),
      });
    },
    [updateBranches],
  );

  const reset = useCallback(() => {
    tokenRef.current += 1; // orphan the running agent; its host calls hang
    agentRef.current = null;
    resolveRef.current = null;
    queueRef.current = [];
    pendingSourceRef.current = null;
    swapFallbackRef.current = null;
    localStorage.removeItem(SAVE_KEY);
    localStorage.removeItem(BRANCH_KEY);
    localStorage.removeItem(HISTORY_KEY);
    branchRef.current = freshBranches();
    setBranchState(branchRef.current);
    historyRef.current = freshHistory();
    setHistory(historyRef.current);
    pendingRestoreRef.current = null;
    applySource(DEFAULT_AGENT_SOURCE);
    setFeed([]);
    setBusy(null);
    setStatusLine('');
    setHasSaved(false);
  }, [applySource]);

  /**
   * Restore a recorded version from the history panel: the stored source
   * goes through the same gate as any edit (compile, replay-validate,
   * hot-swap), and the accepted swap is recorded as a `restore` commit
   * pointing at what it brought back — git-revert semantics, history only
   * ever grows.
   */
  const restoreCommit = useCallback(
    (commit: PlaygroundCommit) => {
      const text = historyRef.current.objects[commit.object];
      if (text === undefined) {
        setStatusLine('Restore failed: that version’s source object is missing.');
        return;
      }
      pendingRestoreRef.current = commit.id;
      const error = applyManualEdit(text);
      if (error) {
        pendingRestoreRef.current = null;
        setStatusLine(`Restore rejected: ${error}`);
      }
    },
    [applyManualEdit],
  );

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
      <div className="mt-8 rounded-xl border border-fd-border bg-fd-card p-6 text-fd-muted-foreground">
        {loadError}
      </div>
    );
  }

  // The user-turn index of each feed event ('user' events only): turn k in
  // the feed is the k-th journaled `input` effect, which is where rewind cuts.
  let userTurns = 0;
  const turnOf = feed.map((e) => (e.kind === 'user' ? userTurns++ : -1));

  // Pair `form` tool events with the `/form <id> {...}` input that answered
  // them — derived from the feed alone, so live chats, restores, replays, and
  // rewinds all agree on which forms are still fillable.
  const forms = pairFormResponses(feed);

  const action =
    'inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-fd-border px-2.5 text-xs font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';
  const segment = (active: boolean) =>
    `rounded-md px-2 py-1 text-xs font-medium transition-colors sm:px-2.5 ${
      active ? 'bg-fd-background shadow-sm' : 'text-fd-muted-foreground hover:text-fd-foreground'
    }`;
  const turnControl =
    'rounded-md border border-fd-border px-2.5 py-1.5 font-mono transition-colors hover:bg-fd-accent hover:text-fd-foreground';

  return (
    <div className="mt-4 sm:mt-6">
      {/*
       * The panel owns all of its chrome — brain picker, actions, timelines,
       * status, composer — and sizes itself against the viewport so on phones
       * it fills the screen below the (static-height) page header and behaves
       * like a chat app: the composer rests at the bottom edge and only the
       * feed scrolls. Anything dynamic lives inside the panel, so the budget
       * in the calc() stays honest.
       */}
      <section
        aria-label="Playground chat"
        className="flex h-[calc(100dvh-16.5rem)] min-h-[20rem] flex-col overflow-hidden rounded-xl border border-fd-border bg-fd-card/50 sm:h-[min(44rem,calc(100dvh-25rem))]"
      >
        <div className="flex items-center gap-2 border-b border-fd-border px-2 py-2 sm:px-3">
          <div className="flex shrink-0 items-center gap-0.5 rounded-lg bg-fd-accent/60 p-0.5" role="group" aria-label="Brain">
            <button id="provider-mock" className={segment(provider === 'mock')} onClick={() => pickProvider('mock')}>
              <span className="sm:hidden">Offline</span>
              <span className="hidden sm:inline">Offline brain</span>
            </button>
            <button
              id="provider-openrouter"
              className={segment(provider === 'openrouter')}
              onClick={() => pickProvider('openrouter')}
            >
              OpenRouter
            </button>
          </div>
          {feed.length > 0 && (
            <span className="ml-auto hidden truncate font-mono text-[11px] text-fd-muted-foreground md:block">
              ⚡ {feed.length} journaled event{feed.length === 1 ? '' : 's'}
              {hasSaved ? ' · saved' : ''}
            </span>
          )}
          <span className="ml-auto flex gap-1.5 md:ml-2">
            <button
              id="replay"
              className={action}
              disabled={!ready || !hasSaved}
              onClick={replay}
              title="Re-render this chat from its journal — zero live calls"
            >
              ↺<span className="hidden sm:inline"> Replay offline</span>
            </button>
            <button
              id="clear"
              className={action}
              disabled={!hasSaved && feed.length === 0 && branchState.stashed.length === 0}
              onClick={reset}
              title="Clear this conversation and every branch"
            >
              ✕<span className="hidden sm:inline"> Reset</span>
            </button>
          </span>
        </div>
        {provider === 'openrouter' && (
          <div className="flex flex-wrap items-center gap-2 border-b border-fd-border bg-fd-accent/30 px-3 py-2">
            {orKey ? (
              <>
                <span id="or-connected" className="shrink-0 text-xs text-fd-muted-foreground">
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
                  className="h-8 w-32 min-w-0 flex-1 rounded-lg border border-fd-border bg-fd-background px-2 text-base sm:max-w-56 sm:text-xs"
                />
                <button id="or-disconnect" className={action} onClick={disconnectOpenRouter}>
                  Disconnect
                </button>
              </>
            ) : (
              <>
                <button id="or-connect" className={action} onClick={connectOpenRouter} disabled={!ready}>
                  Connect OpenRouter
                </button>
                <span className="text-xs text-fd-muted-foreground">
                  Not connected yet — replies use the offline brain.
                </span>
              </>
            )}
          </div>
        )}
        {branchState.stashed.length > 0 && (
          <div
            id="branches"
            className="flex items-center gap-2 overflow-x-auto whitespace-nowrap border-b border-fd-border px-3 py-2 font-mono text-[11px]"
          >
            <span className="shrink-0 rounded-full border border-fd-primary/60 bg-fd-primary/10 px-2.5 py-1 font-medium">
              ⑂ {branchState.activeLabel} · current
            </span>
            {branchState.stashed.map((b) => (
              <span
                key={b.label}
                className="flex shrink-0 items-center overflow-hidden rounded-full border border-fd-border"
              >
                <button
                  id={`switch-${b.label.replace(/\s+/g, '-')}`}
                  className="px-2.5 py-1 transition-colors hover:bg-fd-accent"
                  title={`Switch to “${b.label}” — the current chat is stashed, this branch's blob is restored and replayed`}
                  onClick={() => switchTo(b.label)}
                >
                  ⑂ {b.label} · {b.turns} turn{b.turns === 1 ? '' : 's'}
                </button>
                <button
                  className="border-l border-fd-border px-1.5 py-1 text-fd-muted-foreground transition-colors hover:bg-fd-accent hover:text-fd-foreground"
                  title={`Delete branch “${b.label}”`}
                  aria-label={`Delete branch ${b.label}`}
                  onClick={() => dropBranch(b.label)}
                >
                  ✕
                </button>
              </span>
            ))}
          </div>
        )}

        <div
          ref={feedBoxRef}
          onScroll={onFeedScroll}
          className="chat-scroll min-h-0 flex-1 overflow-y-auto overscroll-contain p-3 sm:p-4"
        >
          {feed.length === 0 && !busy ? (
            <div className="flex h-full flex-col items-center justify-center gap-4 text-center">
              <svg width="28" height="28" viewBox="0 0 24 24" fill="currentColor" aria-hidden className="text-fd-muted-foreground/50">
                <path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z" />
              </svg>
              <div>
                <p className="text-sm font-medium">
                  {ready ? 'Talk to a live chidori agent' : 'Loading the wasm engine…'}
                </p>
                <p className="mx-auto mt-1 max-w-xs text-xs text-fd-muted-foreground">
                  It runs on the wasm engine in this tab — every turn journaled,
                  rewindable, branchable.
                </p>
              </div>
              <ExamplePicker disabled={!ready} onPick={send} />
            </div>
          ) : (
            <div className="flex flex-col gap-3">
              {feed.map((event, i) => {
                if (event.kind === 'user') {
                  const turn = turnOf[i];
                  const formResponse = forms.responses.get(i);
                  return (
                    <div key={i} className="group flex flex-col items-end">
                      {formResponse ? (
                        <FormResponseBubble spec={formResponse.spec} values={formResponse.values} />
                      ) : (
                        <p className="max-w-[85%] whitespace-pre-wrap rounded-2xl rounded-br-md bg-fd-primary px-3.5 py-2 text-sm text-fd-primary-foreground">
                          {event.text}
                        </p>
                      )}
                      <span className="turn-controls mt-1 flex gap-1.5 text-[11px] text-fd-muted-foreground">
                        <button
                          id={`rewind-${turn}`}
                          className={turnControl}
                          title="Rewind here: the journal is truncated just before this message and replayed — later turns on this path are discarded"
                          onClick={() => rewindTo(turn, event.text)}
                        >
                          ⟲ Rewind
                        </button>
                        <button
                          id={`branch-${turn}`}
                          className={turnControl}
                          title="Branch here: stash this conversation as a switchable timeline, then rewind to try a different path"
                          onClick={() => branchFrom(turn, event.text)}
                        >
                          ⑂ Branch
                        </button>
                      </span>
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
                      <ToolCard
                        name={event.name}
                        args={event.args}
                        result={event.result}
                        formHooks={{
                          answered: forms.answers.get(i) ?? null,
                          disabled: !ready,
                          respond: submitForm,
                        }}
                      />
                    </div>
                  );
                }
                return (
                  <p key={i} className="text-center font-mono text-[11px] text-fd-muted-foreground">
                    {event.text}
                  </p>
                );
              })}
              {busy && (
                <p className="flex items-center gap-2 font-mono text-[11px] text-fd-muted-foreground" id="busy">
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
        </div>

        {statusLine && (
          <p
            id="status"
            className="border-t border-fd-border bg-fd-accent/30 px-3 py-1.5 font-mono text-[11px] leading-relaxed text-fd-muted-foreground"
          >
            {statusLine}
          </p>
        )}
        <form
          className="flex items-center gap-2 border-t border-fd-border p-2.5 pb-[max(0.625rem,env(safe-area-inset-bottom))] sm:p-3"
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
            enterKeyHint="send"
            className="h-10 min-w-0 flex-1 rounded-lg border border-fd-border bg-fd-background px-3 text-base outline-none focus:ring-2 focus:ring-fd-primary/40 sm:h-9 sm:text-sm"
          />
          <button
            id="send"
            type="submit"
            className="h-10 shrink-0 rounded-lg bg-fd-primary px-4 text-sm font-medium text-fd-primary-foreground transition-opacity hover:opacity-85 disabled:pointer-events-none disabled:opacity-40 sm:h-9"
            disabled={!ready || !draft.trim()}
          >
            Send
          </button>
        </form>
      </section>

      <details
        className="mt-4 rounded-xl border border-fd-border bg-fd-card/30 p-4 sm:mt-6"
        onToggle={(e) => {
          if (e.currentTarget.open) setHoodOpened(true);
        }}
      >
        <summary className="cursor-pointer text-sm font-medium select-none marker:text-fd-muted-foreground">
          Under the hood
        </summary>
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
            Rewind and branch (the controls under any message you sent) are journal operations: rewinding
            truncates the effect journal just before that turn&apos;s <code>chidori.input()</code>{' '}
            and replays the shorter journal; branching stashes the full blob first, so every
            timeline is just another durable blob you can switch back to.
          </li>
          <li>
            The agent can rewrite itself: <code>read_source</code> and <code>update_source</code>{' '}
            are ordinary tools — and the editor below edits the same live program by hand. An
            accepted edit is validated by replaying this conversation&apos;s journal against the
            new code, then hot-swapped in (modify-and-resume: same journal, new program) — an
            edit that would change already-journaled effect calls is rejected as divergence.
          </li>
          <li>
            Every version that runs is history: each accepted rewrite becomes a commit in the{' '}
            <em>implementation history</em> below — a content-addressed, git-like chain recorded
            alongside the journal, each version anchored to the turns that executed under it.
            The same mechanism records every run&apos;s source history on disk (
            <code>chidori history</code>).
          </li>
          <li>
            Docs answers are grounded: these docs are indexed at build time, retrieved into the
            model&apos;s context, and exposed as the <code>search_docs</code> tool.
          </li>
          <li>
            Inline forms are pure host-side generative UI: the <code>form</code> tool takes a JSON
            Schema, the journaled result is rendered by react-jsonschema-form (so replays repaint
            the form), and submitting sends <code>/form &lt;id&gt; {'{…}'}</code> through the same{' '}
            <code>chidori.input()</code> as any message — the runtime itself knows nothing about
            forms, and rewinding past a submission makes the form fillable again.
          </li>
        </ul>
        <p className="mt-3 text-xs text-fd-muted-foreground" id="source-label">
          agent.ts — the program running this chat, editable
          {source !== DEFAULT_AGENT_SOURCE ? ' · rewritten (ask the agent to "reset your code" to undo)' : ''}
        </p>
        {hoodOpened ? (
          <SourceEditor
            source={source}
            defaultSource={DEFAULT_AGENT_SOURCE}
            busy={busy !== null}
            onApply={applyManualEdit}
          />
        ) : (
          <pre className="mt-1 overflow-x-auto rounded-lg border border-fd-border bg-fd-card p-4 text-xs">
            {source}
          </pre>
        )}
      </details>

      {/*
       * The implementation history: the execution side of this chat lives in
       * the journal (the feed above); this is the code side — the git-like
       * chain of every agent.ts version that ran, mirroring what
       * `chidori history <run-id>` shows for a persisted run.
       */}
      <details className="mt-4 rounded-xl border border-fd-border bg-fd-card/30 p-4">
        <summary className="cursor-pointer text-sm font-medium select-none marker:text-fd-muted-foreground">
          Implementation history
          {history.commits.length > 0 && (
            <span className="ml-2 font-mono text-[11px] font-normal text-fd-muted-foreground">
              {history.commits.length} version{history.commits.length === 1 ? '' : 's'}
            </span>
          )}
        </summary>
        <p className="mt-2 text-xs text-fd-muted-foreground">
          The journal records what this chat <em>did</em>; this chain records what
          the agent <em>was</em> — every source version that ran, as git-like
          commits anchored to the turns executed under each. Ask the agent to
          rewrite its code (or edit it under the hood) to grow the chain; diff any
          two versions; restore an old one and the journal replays against it.
          Timelines (⑂) are branch heads over this one shared history — the same
          model <code>chidori history</code> shows for a persisted run.
        </p>
        <HistoryPanel
          history={history}
          timelineHeads={branchState.stashed.map((b) => ({ label: b.label, head: b.headId }))}
          activeLabel={branchState.activeLabel}
          currentTurns={userTurns}
          busy={busy !== null}
          onRestore={restoreCommit}
        />
      </details>
    </div>
  );
}
