# @1kbirds/chidori-browser

Client-side chidori agents. The pure-Rust engine and its durable replay
runtime run as WebAssembly (built from `crates/chidori-wasm`); this package is
the browser **host**: it presents the `chidori.*` agent API and serves each
journaled host effect with browser primitives — `fetch` for HTTP and LLM
calls, callbacks for `chidori.input()` and tools, timers for `chidori.sleep()`.

Because every effect flows through the journal, browser runs get the full
durability story with **no server at all**:

- suspend at `chidori.input()`, save the run to `localStorage`, close the tab,
  restore and resume days later;
- replay any saved run offline — zero network, zero LLM calls, byte-identical
  output;
- the saved artifact is the same `DurableBlob` the native runtime uses.

## Usage

```html
<script type="module">
  import init, * as wasm from './pkg/chidori_wasm.js';
  import { BrowserAgent, mockLlm, saveRun, loadRun } from './chidori-browser/index.js';

  await init();

  const agent = BrowserAgent.start(wasm, {
    source: `
      async function main() {
        const city = await chidori.input('Which city?');
        const w = await chidori.fetch('https://wttr.in/' + city + '?format=j1');
        const summary = await chidori.prompt('Summarize this weather: ' + w.text);
        await chidori.log(summary);
      }
      main();
    `,
    llm: mockLlm(),                       // or anthropicLlm({ apiKey }) / openaiCompatibleLlm(...)
    onInput: () => undefined,             // undefined → suspend instead of answering
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

Agents may be written in TypeScript — sources are stripped by the wasm
module's oxc transpiler (`filename: 'agent.tsx'` enables JSX). The bundle
executes top-level with the `chidori` global in scope.

## API surface

`chidori.prompt(text, opts)`, `chidori.input(message, opts)`,
`chidori.tool(name, kwargs)`, `chidori.log(message, fields)`,
`chidori.fetch(url, opts)`, `chidori.sleep(ms)`, `chidori.now()`,
`chidori.random()`, `chidori.signal(name, opts)`, and `chidori.step(fn)`
(durable value checkpoint). This is the durable core of the native agent API;
payload shapes match the native host, so journals stay aligned across hosts.
Contexts, conversations, actors, and workspace are native-runtime features and
are not (yet) available client-side.

## LLM access from a page

`anthropicLlm` calls the Anthropic Messages API directly (Anthropic supports
browser calls via the `anthropic-dangerous-direct-browser-access` opt-in
header) — only ever with a key the user typed into the page. For other
providers, or to keep keys out of the client entirely, point
`openaiCompatibleLlm` at a proxy you control, or pass any async
`({ text, opts }) => string` as `llm`. Docs demos should use `mockLlm()`:
deterministic, free, offline.

### OpenRouter, without pasting keys

OpenRouter's API is CORS-enabled and ships a PKCE login flow built for
client-side apps, so users can authenticate with a click instead of handling
keys:

```js
import { startOpenRouterLogin, completeOpenRouterLogin, openRouterLlm } from './chidori-browser/index.js';

// Safe to call unconditionally at startup: null unless this page load is the
// login callback (?code=...).
let key = await completeOpenRouterLogin();

document.querySelector('#connect').onclick = () => startOpenRouterLogin();
// After the redirect back, completeOpenRouterLogin() returns the key:
const agent = BrowserAgent.start(wasm, {
  source,
  llm: openRouterLlm({ apiKey: key, model: 'openrouter/auto', appName: 'My docs demo' }),
});
```

The exchanged key is user-controlled (it lives in their OpenRouter account,
scoped and revocable there). Persist it — e.g. `localStorage` — only if the
user opts in.

## Building the wasm module

```sh
scripts/build-wasm.sh   # produces crates/chidori-wasm/www/pkg/
```

See `crates/chidori-wasm/README.md` for the pump protocol this package sits on.
