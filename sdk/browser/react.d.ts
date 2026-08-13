// Type declarations for @1kbirds/chidori-browser/react (see react.js).

import type { HostOptions, Json } from './index.js';

export type AgentStatus =
  | 'idle'
  | 'running'
  | 'awaiting-input'
  | 'suspended'
  | 'completed'
  | 'error';

export interface AgentView {
  status: AgentStatus;
  /** Console output accumulated so far. */
  console: string[];
  /** `chidori.log()` entries, appended live as the agent runs. */
  logs: { message: string; fields?: Json }[];
  /** The `chidori.input()` question awaiting `controls.answer`, if any. */
  pendingInput: { prompt: string; opts: Json } | null;
  /** Host effects performed live this run (0 on a pure replay). */
  liveCalls: number;
  error: Error | null;
}

export interface AgentControls {
  /** Begin a fresh run of `options.source`. */
  start(overrides?: Partial<{ source: string; filename?: string } & HostOptions>): void;
  /** Resume/replay a saved run from its durable blob. */
  restore(blob: Uint8Array, overrides?: Partial<HostOptions>): void;
  /** Answer the pending `chidori.input()`; the run continues in-page. */
  answer(text: string): void;
  /** Suspend at the pending input; then persist `blob()` for a later restore. */
  suspend(): void;
  /** The durable artifact (bundle + effects + journal), or null before start. */
  blob(): Uint8Array | null;
}

/**
 * Bind a client-side chidori agent to React state: the agent runs on the wasm
 * runtime, React renders in the page, and journaled effects surface as state.
 */
export function useChidoriAgent(
  wasm: unknown,
  options: { source: string; filename?: string } & HostOptions
): [AgentView, AgentControls];
