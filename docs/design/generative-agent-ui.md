# Generative agent UI

An agent that **generates its own interface**: the model produces the UI, that UI
renders through the *same journaled host boundary* Chidori already uses for
`prompt`, `tool`, and `fetch`, and the user's interactions flow back through it.
The reasoning that designed the screen, the LLM call that filled it, the DOM the
agent built, and the clicks the user made all live in one deterministic,
replayable journal.

This is the chapter that sits on top of two pieces already in the tree:

- the **journaled virtual DOM** behind the durable host
  ([`docs/dom-runtime-prototype.md`](../dom-runtime-prototype.md),
  [`crates/chidori-js/src/dom.rs`](../../crates/chidori-js/src/dom.rs)), and
- **real React 18 + `react-dom/server`** executing on the pure-Rust engine
  ([`crates/chidori-js/tests/react_ssr.rs`](../../crates/chidori-js/tests/react_ssr.rs)),
  with `import React from 'react'` resolving to a vendored bundle in the durable
  runtime ([`run_module`](../../crates/chidori/src/runtime/rust_engine.rs)).

The contribution of *generative* UI is the loop that closes over them: a
`chidori.prompt()` call decides what the interface should contain, a trusted
renderer turns that decision into DOM, and `chidori.renderDOM()` flushes it as a
durable `dom_render` effect. Because every input to that loop is journaled, the
generated UI is reproducible: same code + same journal ⇒ byte-identical screen,
for **zero** model calls on replay.

## The core idea: model fills a schema, code renders it

There are two ways to have a model "generate UI", and only one of them is
durable, testable, and safe:

| Approach | What the model returns | Problem |
| --- | --- | --- |
| **Code-gen** | raw JSX/JS to `eval` | unsandboxed, non-deterministic to test, a new injection surface |
| **Schema-fill** (this design) | a typed JSON description of the screen | the renderer is fixed, trusted code; the model only chooses *content and structure within a contract* |

So a generative UI screen is a **pure function of a journaled JSON spec**. The
model's only job is to emit a value that satisfies a schema (`UiSpec`); a trusted
React component (`Screen`) renders that value the same way every time. This is
the same shape as structured tool-calling — the model fills a contract, code
does the effecting — applied to the interface instead of a tool.

```
 input ──▶ chidori.prompt(schema)  ──▶  UiSpec (JSON, journaled)
                                          │
                              Screen(spec)│  trusted React component
                                          ▼
                       react-dom/server → innerHTML → journaled DOM
                                          │
                                 chidori.renderDOM()  ──▶  dom_render effect
```

Everything left of `renderDOM()` is a deterministic re-derivation on replay; the
single non-deterministic step (the model call) is served from the journal.

## What this change adds

- **Design doc** — this file: the schema-fill model, the determinism property,
  and the phased plan below.
- **A runnable generative-UI agent** —
  [`examples/agents/generative_ui.tsx`](../../examples/agents/generative_ui.tsx).
  It prompts the model for a `UiSpec` describing a screen, renders it with a
  trusted `Screen` component through `react-dom/server`, mounts the markup into
  the journaled DOM, and flushes a durable `dom_render`. It returns the rendered
  HTML plus a summary of the render batch. JSON-fence tolerant, so it survives a
  model that wraps its answer in ```` ```json ````.
- **A deterministic end-to-end test** —
  `generative_ui_agent_renders_a_prompted_spec_and_journals_the_render` in
  [`rust_engine.rs`](../../crates/chidori/src/runtime/rust_engine.rs). It seeds
  the `prompt` result in a replay log (the model's "generated" spec), runs the
  `.tsx` agent through the real durable runtime, and asserts: the spec rendered
  to the expected DOM, a `dom_render` effect was journaled, and re-running the
  same journal is byte-identical. No live provider, no node_modules.

## The determinism property (why it matters here)

The DOM prototype already proves *its* runs are deterministic given a session
journal. Generative UI inherits that and adds one edge: the **content** of the
UI now comes from a model call, which is itself a journaled host effect. So the
guarantee composes end to end:

> same agent + same call log (the prompt's `UiSpec` + any events)
> ⇒ byte-identical rendered HTML + byte-identical `dom_render` batch.

That is what makes a generated interface checkable. You can commit the journal of
a run and assert in CI that the agent still produces the same screen — a UI
regression test that costs `$0` and re-bills no tokens — and you can fork at any
interaction, edit the generating prompt or the `Screen` renderer, and re-run with
all upstream model work replayed for free.

## How it slots into the durable host

Nothing new at the boundary — generative UI is three host effects the runtime
already routes, used together:

1. **`prompt`** decides the content. Recorded live, served from the journal on
   replay. The result is the `UiSpec`.
2. **DOM writes** build the tree. Pure virtual-tree ops in the engine; nothing
   journaled until you flush.
3. **`dom_render`** ships the batch. `chidori.renderDOM()` drains the pending
   mutations through `dispatch("dom_render", batch)` — recorded live, a no-op
   served from the journal on replay.

(Inbound UI **events** are the fourth direction — already modelled as a captured
host input in the DOM prototype — and are what a *stateful* generative screen
will consume in P2 below.)

## Phased plan

**P0 — generate-and-render (DONE, this change).** Model fills a `UiSpec`; a
trusted `Screen` renders it; the result mounts into the journaled DOM and flushes
a durable `dom_render`. Deterministic, replayable, tested end to end through the
real runtime.

**P1 — typed spec + validation.** Promote `UiSpec` to a shared schema the agent
can hand to the model as a tool/response contract, and validate the model's
output against it before rendering (reject/repair on miss), so a malformed
generation fails loudly at the boundary instead of rendering garbage. Ships with
the `Screen` component covering a small but real widget set (heading, prose,
list, key/value, button, badge).

**P2 — interactive, stateful screens.** Wire inbound DOM **events** (the captured
input stream the prototype already journals) into an agent turn: a click flushes
an event, the agent re-prompts with the interaction in context, and the next
`UiSpec` re-renders. Now the whole session — generation, interaction,
re-generation — is one causal, forkable log (Phoenix-LiveView-shaped, with
Replay.io-style determinism and a branch model neither has).

**P3 — iterate-against-tests.** Fold in the agent-iterates-on-React loop
([`examples/react_agent_demo.rs`](../../crates/chidori-js/examples/react_agent_demo.rs)):
a DOM-query acceptance suite gates each generated screen, failing assertions
drive the next generation, and fork → edit → replay reconstructs a green state
for free before making one fresh call for an edit.

## Deliberately deferred (rationale, not blockers)

- **Client-side hydration / interactivity in a browser.** This design is
  server-authoritative: the engine renders, the browser displays and reports
  events. Shipping a real browser client that hydrates the journaled DOM is a
  separate transport concern, not engine work.
- **Free-form code generation.** Letting the model emit arbitrary component code
  (rather than fill a schema) trades the determinism/safety story for
  flexibility; if needed it belongs behind an explicit, sandboxed, opt-in path,
  not the default.
- **60fps production client apps.** The from-scratch RC-GC interpreter is below
  V8 — right for agent-driven iteration and server-authoritative diffing, not a
  high-framerate client runtime. Inherent to the engine, not this layer.

## The artifact, in one line

An agent that designs and renders its own interface through the durable host —
where the model's choice of screen, the DOM it produced, and the user's clicks
share one replayable journal, so any generated UI is reproducible, forkable, and
testable for `$0`.
