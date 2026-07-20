'use client';

/**
 * The interactive part of /playground: loads the wasm engine and the browser
 * SDK at runtime from /public (they are build artifacts produced by
 * scripts/build-wasm.sh, deliberately outside the Next bundle — hence the
 * webpackIgnore'd dynamic imports), then drives a durable agent through
 * record → suspend → resume → offline replay.
 */

import { useCallback, useEffect, useRef, useState } from 'react';

// Inlined at build time from DOCS_BASE_PATH (see next.config.mjs) so asset
// URLs work both locally ('') and on GitHub Pages ('/chidori').
const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
const ASSETS = `${BASE}/chidori-wasm`;
const SAVE_KEY = 'chidori-playground-run';
// The exchanged OpenRouter key lives in sessionStorage: it survives the PKCE
// redirect back to this page, and is gone when the tab closes.
const OR_KEY = 'chidori-playground-openrouter-key';

const SAMPLE_AGENT = `interface Weather { tempC: number; sky: string }

async function main(): Promise<void> {
  await chidori.log('starting research');
  const city = await chidori.input('Which city should I research?');
  const weather = await chidori.tool('weather', { city }) as Weather;
  const fact = await chidori.fetch('${ASSETS}/fact.json');
  const stamp = await chidori.now();
  const summary = await chidori.prompt(
    \`One line on \${city}: \${weather.tempC}C, \${weather.sky}\`);
  console.log(\`city: \${city}\`);
  console.log(\`weather: \${weather.tempC}C, \${weather.sky}\`);
  console.log(\`fact: \${fact.json.text}\`);
  console.log(\`summary: \${summary}\`);
  console.log(\`researched at \${stamp}\`);
}
main();
`;

interface RunView {
  status: string;
  console: string[];
  liveCalls: number;
  pendingInput?: { prompt: string };
}

declare global {
  interface Window {
    /** Set once the wasm assets are loaded; the e2e tests key off these. */
    __chidoriReady?: boolean;
    __lastRun?: RunView;
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
  const [source, setSource] = useState(SAMPLE_AGENT);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [statusLine, setStatusLine] = useState('');
  const [consoleLines, setConsoleLines] = useState<string[]>([]);
  const [hostLines, setHostLines] = useState<string[]>([]);
  const [question, setQuestion] = useState<string | null>(null);
  const [answer, setAnswer] = useState('');
  const [hasSaved, setHasSaved] = useState(false);
  const [provider, setProvider] = useState<'mock' | 'openrouter'>('mock');
  const [orKey, setOrKey] = useState<string | null>(null);
  const [model, setModel] = useState('openrouter/auto');
  const loadedRef = useRef<Loaded | null>(null);
  const agentRef = useRef<{ run(): Promise<RunView>; blob(): Uint8Array } | null>(null);
  const askResolveRef = useRef<((answer: string) => void) | null>(null);

  useEffect(() => {
    setHasSaved(localStorage.getItem(SAVE_KEY) !== null);
    // Preload so the first click is instant; surface a friendly error if the
    // assets have not been built into public/chidori-wasm.
    loadAssets()
      .then(async (loaded) => {
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
          setOrKey(key);
          setProvider('openrouter');
        }
        window.__chidoriReady = true;
      })
      .catch(() =>
        setLoadError(
          'The wasm assets are missing. Build them with scripts/build-wasm.sh, then reload.',
        ),
      );
  }, []);

  const host = useCallback(
    (interactive: boolean) => ({
      llm:
        provider === 'openrouter' && orKey && loadedRef.current
          ? loadedRef.current.sdk.openRouterLlm({
              apiKey: orKey,
              model,
              appName: 'Chidori Playground',
              appUrl: typeof location !== 'undefined' ? location.origin : undefined,
            })
          : async ({ text }: { text: string }) =>
              text.startsWith('One line on')
                ? 'crisp skies, bring a light jacket'
                : `[mock] ${text}`,
      tools: {
        weather: (kwargs: unknown) => {
          const { city } = kwargs as { city: string };
          return { tempC: 11 + city.length, sky: 'clear' };
        },
      },
      onLog: ({ message }: { message: string }) =>
        setHostLines((l) => [...l, `log: ${message}`]),
      onInput: ({ prompt }: { prompt: string }) => {
        if (!interactive) return undefined; // suspend: savable frontier
        setQuestion(prompt);
        return new Promise<string>((resolve) => {
          askResolveRef.current = resolve;
        });
      },
    }),
    [provider, orKey, model],
  );

  const connectOpenRouter = useCallback(() => {
    // Redirects to openrouter.ai's consent page; the redirect back lands on
    // this page with ?code=, which the mount effect exchanges for a key.
    loadedRef.current?.sdk.startOpenRouterLogin();
  }, []);

  const disconnectOpenRouter = useCallback(() => {
    sessionStorage.removeItem(OR_KEY);
    setOrKey(null);
    setProvider('mock');
  }, []);

  const finish = useCallback((result: RunView, mode: string) => {
    setConsoleLines(result.console);
    if (agentRef.current) {
      localStorage.setItem(
        SAVE_KEY,
        new TextDecoder().decode(agentRef.current.blob()),
      );
    }
    setHasSaved(true);
    if (result.status === 'suspended') {
      setStatusLine(
        `⏸ Suspended at input("${result.pendingInput?.prompt}") — saved to localStorage. Reload this page, then hit Resume.`,
      );
    } else {
      setStatusLine(`✅ Completed — ${mode}: ${result.liveCalls} live host call(s).`);
      setHostLines((l) => [...l, `— ${mode}: ${result.liveCalls} live host call(s) —`]);
    }
    window.__lastRun = result;
  }, []);

  const begin = useCallback(() => {
    setBusy(true);
    setStatusLine('');
    setConsoleLines([]);
    setHostLines([]);
    setQuestion(null);
  }, []);

  const run = useCallback(async () => {
    const loaded = loadedRef.current;
    if (!loaded) return;
    begin();
    try {
      const agent = loaded.sdk.BrowserAgent.start(loaded.wasm, {
        source,
        ...host(false),
      });
      agentRef.current = agent;
      finish((await agent.run()) as RunView, 'record');
    } catch (err) {
      setStatusLine(`Error: ${String(err)}`);
    } finally {
      setBusy(false);
    }
  }, [source, host, begin, finish]);

  const resume = useCallback(async () => {
    const loaded = loadedRef.current;
    const saved = localStorage.getItem(SAVE_KEY);
    if (!loaded || saved === null) return;
    begin();
    try {
      const agent = loaded.sdk.BrowserAgent.restore(
        loaded.wasm,
        new TextEncoder().encode(saved),
        host(true),
      );
      agentRef.current = agent;
      finish((await agent.run()) as RunView, 'resume');
    } catch (err) {
      setStatusLine(`Error: ${String(err)}`);
    } finally {
      setBusy(false);
    }
  }, [host, begin, finish]);

  const replay = useCallback(async () => {
    const loaded = loadedRef.current;
    const saved = localStorage.getItem(SAVE_KEY);
    if (!loaded || saved === null) return;
    begin();
    try {
      // No host at all: every effect must come from the journal.
      const agent = loaded.sdk.BrowserAgent.restore(
        loaded.wasm,
        new TextEncoder().encode(saved),
        {
          llm: () => {
            throw new Error('replay must not call the LLM');
          },
          fetchImpl: () => {
            throw new Error('replay must not touch the network');
          },
        },
      );
      agentRef.current = agent;
      finish((await agent.run()) as RunView, 'replay');
    } catch (err) {
      setStatusLine(`Error: ${String(err)}`);
    } finally {
      setBusy(false);
    }
  }, [begin, finish]);

  const sendAnswer = useCallback(() => {
    setQuestion(null);
    askResolveRef.current?.(answer);
    askResolveRef.current = null;
  }, [answer]);

  const discard = useCallback(() => {
    localStorage.removeItem(SAVE_KEY);
    setHasSaved(false);
    setStatusLine('Saved run discarded.');
  }, []);

  if (loadError) {
    return (
      <div className="mt-8 rounded-lg border border-fd-border bg-fd-card p-6 text-fd-muted-foreground">
        {loadError}
      </div>
    );
  }

  const button =
    'rounded-lg border border-fd-border px-4 py-2 text-sm font-medium transition-colors hover:bg-fd-accent disabled:pointer-events-none disabled:opacity-40';

  return (
    <div className="mt-8">
      <textarea
        value={source}
        onChange={(e) => setSource(e.target.value)}
        spellCheck={false}
        aria-label="Agent source (TypeScript)"
        className="min-h-[22rem] w-full rounded-lg border border-fd-border bg-fd-card p-4 font-mono text-sm"
      />
      <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-2 text-sm">
        <span className="font-medium">LLM for chidori.prompt():</span>
        <label className="flex items-center gap-1.5">
          <input
            type="radio"
            name="provider"
            id="provider-mock"
            checked={provider === 'mock'}
            onChange={() => setProvider('mock')}
          />
          Deterministic mock (offline, free)
        </label>
        <label className="flex items-center gap-1.5">
          <input
            type="radio"
            name="provider"
            id="provider-openrouter"
            checked={provider === 'openrouter'}
            onChange={() => setProvider('openrouter')}
          />
          OpenRouter (real models)
        </label>
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
                onChange={(e) => setModel(e.target.value)}
                aria-label="OpenRouter model"
                className="w-56 rounded-lg border border-fd-border bg-fd-background px-2 py-1"
              />
              <button id="or-disconnect" className={button} onClick={disconnectOpenRouter}>
                Disconnect
              </button>
            </span>
          ) : (
            <button id="or-connect" className={button} onClick={connectOpenRouter}>
              Connect OpenRouter
            </button>
          ))}
      </div>
      <div className="mt-4 flex flex-wrap gap-3">
        <button id="run" className={button} disabled={busy || (provider === 'openrouter' && !orKey)} onClick={run}>
          ▶ Run agent
        </button>
        <button id="resume" className={button} disabled={busy || !hasSaved} onClick={resume}>
          ⏯ Resume saved run
        </button>
        <button id="replay" className={button} disabled={busy || !hasSaved} onClick={replay}>
          ↺ Replay offline
        </button>
        <button id="clear" className={button} disabled={busy || !hasSaved} onClick={discard}>
          ✕ Discard saved run
        </button>
      </div>

      {question !== null && (
        <div
          id="ask"
          className="mt-4 rounded-lg border border-fd-border bg-fd-accent/50 p-4"
        >
          <label htmlFor="answer" className="block text-sm font-medium">
            {question}
          </label>
          <div className="mt-2 flex gap-2">
            <input
              id="answer"
              type="text"
              value={answer}
              onChange={(e) => setAnswer(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && sendAnswer()}
              className="w-64 rounded-lg border border-fd-border bg-fd-background px-3 py-1.5 text-sm"
            />
            <button id="send" className={button} onClick={sendAnswer}>
              Answer
            </button>
          </div>
        </div>
      )}

      {statusLine && <p id="status" className="mt-4 font-medium">{statusLine}</p>}

      <div className="mt-6 grid gap-4 md:grid-cols-2">
        <section>
          <h2 className="text-sm font-semibold">Agent console</h2>
          <pre
            id="console"
            className="mt-2 min-h-[8rem] rounded-lg border border-fd-border bg-fd-card p-4 text-xs whitespace-pre-wrap"
          >
            {consoleLines.join('\n')}
          </pre>
        </section>
        <section>
          <h2 className="text-sm font-semibold">Host log</h2>
          <pre
            id="hostlog"
            className="mt-2 min-h-[8rem] rounded-lg border border-fd-border bg-fd-card p-4 text-xs whitespace-pre-wrap"
          >
            {hostLines.join('\n')}
          </pre>
        </section>
      </div>
    </div>
  );
}
