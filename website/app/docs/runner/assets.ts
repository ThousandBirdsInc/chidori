'use client';

/**
 * Shared loaders for the runnable-docs machinery: the wasm engine + browser
 * SDK (static assets built by scripts/build-wasm.sh) and the playground's
 * docs search index. One promise each — the panel, the VM terminal, and the
 * fake session server all share the same instances.
 */

import { type DocsIndex, prepareDocsIndex } from '../../(home)/playground/brain';
import type { EngineAssets } from './run-host';

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
const ASSETS = `${BASE}/chidori-wasm`;

let assetsPromise: Promise<EngineAssets> | null = null;

export function loadEngine(): Promise<EngineAssets> {
  // Runtime imports on purpose (same as the playground): the wasm module and
  // SDK are static assets, not bundle modules.
  assetsPromise ??= (async () => {
    const wasm = await import(/* webpackIgnore: true */ `${ASSETS}/chidori_wasm.js`);
    await wasm.default();
    const sdk = await import(/* webpackIgnore: true */ `${ASSETS}/chidori-browser.js`);
    return { wasm, sdk } as EngineAssets;
  })();
  return assetsPromise;
}

let docsIndexPromise: Promise<DocsIndex | null> | null = null;

export function loadDocsIndex(): Promise<DocsIndex | null> {
  docsIndexPromise ??= fetch(`${BASE}/playground-docs.json`)
    .then((res) => (res.ok ? res.json() : null))
    .then((json) => (json ? prepareDocsIndex(json) : null))
    .catch(() => null);
  return docsIndexPromise;
}
