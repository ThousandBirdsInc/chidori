// @1kbirds/chidori-browser/react — React bindings for client-side chidori
// agents.
//
// The division of labor that makes this fast: React runs in the page on the
// browser's native engine (real DOM, real reconciler), and the chidori agent
// runs behind it on the wasm runtime — the interpreter only ever executes
// agent logic, never UI rendering. The hook binds the two: agent progress
// (logs, console, pending input, completion) becomes React state, and user
// interaction feeds the agent's journaled `chidori.input()` effects.
//
// Because every effect still flows through the journal, the durability story
// survives the UI: suspend mid-conversation, `saveRun` the blob, restore it
// in a fresh tab — the same hook picks the run back up at the same question.
//
// Plain ESM + JSDoc like the rest of the package; `react` is a peer import
// (any React ≥ 16.8 with hooks).

import { useCallback, useEffect, useRef, useState } from 'react';
import { BrowserAgent } from './index.js';

/**
 * @typedef {Object} AgentView
 * @property {'idle'|'running'|'awaiting-input'|'suspended'|'completed'|'error'} status
 * @property {string[]} console          - console output accumulated so far
 * @property {{ message: string, fields?: * }[]} logs - `chidori.log()` entries, live
 * @property {{ prompt: string, opts: * } | null} pendingInput - question awaiting {@link AgentControls.answer}
 * @property {number} liveCalls          - host effects performed live (0 on a pure replay)
 * @property {Error | null} error
 */

/**
 * @typedef {Object} AgentControls
 * @property {(overrides?: object) => void} start - begin a fresh run of `options.source`
 * @property {(blob: Uint8Array) => void} restore - resume/replay a saved run
 * @property {(text: string) => void} answer      - answer the pending `chidori.input()`
 * @property {() => void} suspend                 - suspend at the pending input (then save {@link AgentControls.blob})
 * @property {() => Uint8Array | null} blob       - the durable artifact, savable anywhere
 */

/**
 * Bind a client-side chidori agent to React state.
 *
 * ```jsx
 * const [agent, controls] = useChidoriAgent(wasm, { source, llm: mockLlm() });
 * // ...
 * {agent.pendingInput && (
 *   <form onSubmit={(e) => { e.preventDefault(); controls.answer(text); }}>
 *     <label>{agent.pendingInput.prompt}</label>
 *     <input value={text} onChange={(e) => setText(e.target.value)} />
 *   </form>
 * )}
 * ```
 *
 * The run is NOT started automatically — call `controls.start()` (or
 * `controls.restore(blob)`), typically from a button or an effect. While the
 * agent awaits `chidori.input()`, `status` is `'awaiting-input'` and the run
 * stays live inside the page; `controls.answer(text)` feeds the journaled
 * effect and the pump continues. `controls.suspend()` instead completes the
 * pump with a suspension so the blob can be persisted and restored later —
 * including in a different tab, days later, offline.
 *
 * @param {*} wasm - the initialized chidori_wasm module
 * @param {{ source: string, filename?: string } & import('./index.js').HostOptions} options
 * @returns {[AgentView, AgentControls]}
 */
export function useChidoriAgent(wasm, options) {
  const [view, setView] = useState(/** @type {AgentView} */ ({
    status: 'idle',
    console: [],
    logs: [],
    pendingInput: null,
    liveCalls: 0,
    error: null,
  }));

  // Everything that must survive re-renders without retriggering them:
  // the live agent, the resolver for the input the UI is being asked for,
  // and the latest options (so `start` in a callback sees current props
  // without being re-created per render).
  const ref = useRef({
    agent: /** @type {BrowserAgent | null} */ (null),
    resolveInput: /** @type {((answer: string | undefined) => void) | null} */ (null),
    alive: true,
    /** Run generation: a new start/restore obsoletes the previous pump's
     * state updates, so an abandoned run can never clobber the live one. */
    gen: 0,
    options,
  });
  ref.current.options = options;

  useEffect(() => {
    const cell = ref.current;
    cell.alive = true;
    return () => {
      cell.alive = false;
      // A pump parked on user input would otherwise hold the closure forever.
      cell.resolveInput?.(undefined);
    };
  }, []);

  const patch = useCallback((gen, partial) => {
    if (ref.current.alive && ref.current.gen === gen) setView((v) => ({ ...v, ...partial }));
  }, []);

  const pump = useCallback(
    /** @param {BrowserAgent} agent @param {number} gen */
    async (agent, gen) => {
      ref.current.agent = agent;
      patch(gen, { status: 'running', error: null, pendingInput: null, logs: [], liveCalls: 0 });
      try {
        const result = await agent.run();
        patch(gen, {
          status: result.status, // 'completed' | 'suspended'
          console: result.console,
          liveCalls: result.liveCalls,
          pendingInput: result.status === 'suspended' ? result.pendingInput : null,
        });
      } catch (err) {
        patch(gen, { status: 'error', error: err instanceof Error ? err : new Error(String(err)) });
      }
    },
    [patch]
  );

  /** Host wrapper: journaled effects surface as React state transitions. */
  const buildHost = useCallback(
    (overrides, gen) => {
      const opts = { ...ref.current.options, ...overrides };
      return {
        ...opts,
        onLog: (payload) => {
          opts.onLog?.(payload);
          if (ref.current.alive && ref.current.gen === gen) {
            setView((v) => ({
              ...v,
              logs: [...v.logs, payload],
              console: ref.current.agent?.console() ?? v.console,
            }));
          }
        },
        onInput: (payload) =>
          new Promise((resolve) => {
            if (ref.current.gen !== gen) return resolve(undefined); // abandoned run
            ref.current.resolveInput = (answer) => {
              ref.current.resolveInput = null;
              resolve(answer);
            };
            patch(gen, {
              status: 'awaiting-input',
              pendingInput: payload,
              console: ref.current.agent?.console() ?? [],
            });
          }),
      };
    },
    [patch]
  );

  const start = useCallback(
    (overrides) => {
      const gen = ++ref.current.gen;
      ref.current.resolveInput?.(undefined); // release a pump parked on input
      const { source, filename, ...host } = { ...ref.current.options, ...overrides };
      const agent = BrowserAgent.start(wasm, { source, filename, ...buildHost(host, gen) });
      void pump(agent, gen);
    },
    [wasm, buildHost, pump]
  );

  const restore = useCallback(
    (blob, overrides) => {
      const gen = ++ref.current.gen;
      ref.current.resolveInput?.(undefined); // release a pump parked on input
      const { source: _s, filename: _f, ...host } = { ...ref.current.options, ...overrides };
      const agent = BrowserAgent.restore(wasm, blob, buildHost(host, gen));
      void pump(agent, gen);
    },
    [wasm, buildHost, pump]
  );

  const answer = useCallback(
    (text) => {
      const resolve = ref.current.resolveInput;
      if (!resolve) return;
      patch(ref.current.gen, { status: 'running', pendingInput: null });
      resolve(String(text));
    },
    [patch]
  );

  const suspend = useCallback(() => {
    // Answering `undefined` makes the pump return `{ status: 'suspended' }`,
    // at which point the blob carries the full run up to this question.
    ref.current.resolveInput?.(undefined);
  }, []);

  const blob = useCallback(() => ref.current.agent?.blob() ?? null, []);

  return [view, { start, restore, answer, suspend, blob }];
}
