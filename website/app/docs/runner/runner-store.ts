'use client';

/**
 * The tiny global store connecting docs code blocks to the example-runner
 * side panel: any RunnablePre on the page can open the panel with its code,
 * and the panel (mounted once in the docs layout) subscribes here.
 */

export interface RunnableInfo {
  /** ts/js blocks run on the wasm engine; shell blocks play in the VM terminal. */
  mode: 'program' | 'fragment' | 'shell';
  /** Editable-input template for program-mode examples ({} when absent). */
  input?: Record<string, unknown>;
  /** Build-time stand-ins for identifiers the docs prose establishes around a fragment. */
  ambient?: string;
}

export interface RunnableExample extends RunnableInfo {
  /** Content hash — identifies the example across opens. */
  id: string;
  /** The block's exact source text, as displayed. */
  code: string;
  /** The docs page the block came from (for the panel header). */
  title: string;
}

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

let current: RunnableExample | null = null;
const listeners = new Set<() => void>();

export function openRunner(example: RunnableExample): void {
  current = example;
  for (const l of listeners) l();
}

export function closeRunner(): void {
  current = null;
  for (const l of listeners) l();
}

export function subscribeRunner(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => listeners.delete(onChange);
}

export function getRunnerExample(): RunnableExample | null {
  return current;
}

// ---------------------------------------------------------------------------
// Runnable-block lookup: public/runnable-examples.json is built from docs/
// by scripts/build-runnable-examples.mjs and keyed by an FNV-1a hash of the
// block's normalized text (same function as the build script).

export function hashCode(code: string): string {
  const s = code.replace(/\r\n/g, '\n').trim();
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0).toString(36);
}

let indexPromise: Promise<Record<string, RunnableInfo>> | null = null;

function loadIndex(): Promise<Record<string, RunnableInfo>> {
  indexPromise ??= fetch(`${BASE}/runnable-examples.json`)
    .then((res) => (res.ok ? res.json() : { examples: {} }))
    .then((json: { examples?: Record<string, RunnableInfo> }) => json.examples ?? {})
    .catch(() => ({}));
  return indexPromise;
}

/** Resolve a code block to its runnable entry, or null for static blocks. */
export async function lookupRunnable(code: string): Promise<(RunnableInfo & { id: string }) | null> {
  const id = hashCode(code);
  const index = await loadIndex();
  const info = index[id];
  return info ? { ...info, id } : null;
}
