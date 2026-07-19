---
title: "Browser Agents"
description: "Client-side-only chidori agents: the wasm engine, the browser SDK, suspend/resume via localStorage, offline replay, and OpenRouter PKCE auth."
---

# Client-Side Agents in the Browser

The chidori engine — bytecode compiler, VM, GC, and the durable replay
runtime — is pure Rust, and it compiles to WebAssembly. That means a chidori
agent can run **entirely client-side**: no server, no Node, no keys on your
infrastructure. Try it on the [playground](/playground/).

The architecture is the same one the native runtime uses. Every side effect
flows through the journaled host-call boundary; only the *host* changes. In
the browser, the host is the page: `fetch` serves HTTP and LLM calls, a
callback serves `chidori.input()`, plain JS functions serve tools. Because
every effect is journaled, browser runs get the full durability story:

- **Suspend at `chidori.input()`** — save the run to `localStorage` (or
  anywhere), close the tab, restore and resume days later, exactly at the
  frontier.
- **Replay offline** — re-run any saved run with zero live host calls, zero
  network requests, and byte-identical output.
- **One artifact** — the saved blob is the same `DurableBlob` the native
  runtime reads and writes.

## The pieces

- [`crates/chidori-wasm`](../crates/chidori-wasm/) — the engine and replay
  runtime behind a small wasm-bindgen boundary, plus `stripTypes()` (the same
  oxc TypeScript stripping the native runtime uses, so browser agents are
  written in TS).
- [`sdk/browser`](../sdk/browser/) — `@1kbirds/chidori-browser`, a no-build
  ESM package that presents the `chidori.*` agent API over the runtime and
  implements the host effects with browser primitives.

Build the assets with [`scripts/build-wasm.sh`](../scripts/build-wasm.sh).

## A minimal client-side agent

```html
<script type="module">
  import init, * as wasm from './pkg/chidori_wasm.js';
  import { BrowserAgent, mockLlm, saveRun, loadRun } from './pkg/chidori-browser.js';

  await init();

  const agent = BrowserAgent.start(wasm, {
    source: `
      async function main() {
        const city = await chidori.input('Which city?');
        const summary = await chidori.prompt('One line about ' + city);
        await chidori.log(summary);
      }
      main();
    `,
    llm: mockLlm(),
    onInput: () => undefined, // undefined → suspend instead of answering
  });

  const r = await agent.run();
  if (r.status === 'suspended') saveRun('my-run', agent.blob());

  // …later, possibly in a fresh tab:
  const resumed = BrowserAgent.restore(wasm, loadRun('my-run'), {
    onInput: ({ prompt }) => window.prompt(prompt) ?? undefined,
  });
  await resumed.run();
</script>
```

The agent surface is the durable core of the [host API](./host-api.md):
`chidori.prompt`, `chidori.input`, `chidori.tool`, `chidori.log`,
`chidori.fetch`, `chidori.sleep`, `chidori.now`, `chidori.random`,
`chidori.signal`, and `chidori.step`. Payload shapes match the native host.
Contexts, conversations, actors, and workspace are native-runtime features
and are not available client-side.

## Real LLMs from a page

Three options, in increasing order of polish:

1. **A user-supplied Anthropic key** — `anthropicLlm({ apiKey })` calls the
   Messages API directly; Anthropic explicitly supports browser calls via the
   `anthropic-dangerous-direct-browser-access` opt-in header.
2. **A proxy you control** — `openaiCompatibleLlm({ baseUrl, model })` points
   at any OpenAI-compatible endpoint (LiteLLM, a worker that holds the key).
3. **OpenRouter with PKCE login** — no keys pasted at all:

```js
import {
  startOpenRouterLogin, completeOpenRouterLogin, openRouterLlm,
} from './pkg/chidori-browser.js';

// Null unless this page load is the login callback — safe to call always.
let key = await completeOpenRouterLogin();
connectButton.onclick = () => startOpenRouterLogin();

const agent = BrowserAgent.start(wasm, {
  source,
  llm: openRouterLlm({ apiKey: key, appName: 'My app' }),
});
```

The user authenticates on openrouter.ai and is redirected back with a
one-time code; the SDK exchanges it (S256 code challenge) for a key that
lives in — and is revocable from — the user's own OpenRouter account.

## What stays native

The browser build swaps the host, not the harness. The native runtime's
process isolation, SQLite stores, `chidori serve`, actors, and detached
agents remain server-side features — in a tab, the browser itself is the
sandbox, `localStorage`/IndexedDB is the store, and the page is the event
loop. See [Architecture](./architecture.md) for the host-call boundary that
makes the two hosts interchangeable.
