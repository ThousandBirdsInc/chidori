# DOM behind the host boundary

Put a DOM behind the *same journaled host boundary* that Chidori already uses
for `prompt`, `tool`, and `fetch`, and you get a UI runtime where the agent's
reasoning, its LLM/tool calls, the resulting DOM mutations, and the user's
interactions all live in one deterministic, replayable journal.

It lives in [`crates/chidori-js/src/dom.rs`](../crates/chidori-js/src/dom.rs),
with tests in
[`tests/dom_prototype.rs`](../crates/chidori-js/tests/dom_prototype.rs) (core
journals),
[`tests/dom_serious.rs`](../crates/chidori-js/tests/dom_serious.rs) (full event
model, captured measurements, DOM API, no-leak), and
[`tests/dom_host_integration.rs`](../crates/chidori-js/tests/dom_host_integration.rs)
(one shared causal journal), plus a runnable demo in
[`examples/dom_session.rs`](../crates/chidori-js/examples/dom_session.rs)
(`cargo run -p chidori-js --example dom_session`).

## Feature status (P0 / P1)

What's wired for building on top of (vs. the engine-only prototype):

- **P0 — durable-host integration: DONE.** `run_module` (`src/runtime/rust_engine.rs`)
  installs the DOM for every agent: `document` / `window` are available, and
  `chidori.renderDOM()` flushes the pending mutation batch through the host
  boundary as a journaled `dom_render` effect (recorded via the call-log, served
  from the journal on replay). The DOM tree is a pure re-derivation of the re-run,
  so node ids stay deterministic across resume. Tested end-to-end
  (`dom_is_available_and_render_effect_is_journaled_and_replayed`); 257 lib + 16
  CLI tests stay green (additive).
- **P1 — DOM completeness: DONE.** Real CSS selector engine (lists, `>`/descendant
  combinators, attribute selectors, `:first/last/nth-child`), `insertAdjacentHTML`,
  `normalize`, `innerHTML` parser.
- **P1 — versioned wire format: DONE.** `PROTOCOL_VERSION`, `RenderBatch`,
  `SessionJournal.version`.
- **P1 — JSX authoring: DONE.** `chidori_js::jsx::transpile_jsx` lowers JSX/TSX to
  classic `React.createElement` (+ strips TS), and the runtime transpile emits
  classic JSX for `.tsx` agents. Kept out of the conformance-critical default
  compile path.
- **P1 — npm package resolution: DONE.** `import React from 'react'` and
  `import { renderToStaticMarkup } from 'react-dom/server'` resolve through a
  built-in vendored-package registry
  (`crates/chidori/src/runtime/typescript/builtins.rs::vendored_module`, resolution
  wired in `crates/chidori/src/runtime/rust_engine.rs`): the
  self-contained UMD bundles are wrapped as synthetic ES modules (the same
  mechanism as the `node:` shims), so they link on the ESM-only engine without a
  `node_modules` install. (npm `react` is CommonJS with internal `require`, which
  can't link directly — the UMD is self-contained, so that's what we serve.)
  Tested end to end: a `.tsx` agent imports React + react-dom/server, authors
  JSX, and renders through the real runtime (`agent_imports_react_and_renders_jsx`).

Deliberately deferred (rationale, not blockers):

- **General CommonJS `node_modules` interop.** Arbitrary CJS packages with
  internal `require()` aren't linked (the engine is ESM-only). Self-contained
  ESM/UMD packages work; full CJS interop (a synchronous `require` graph) is a
  larger, separate feature.
- **Ask/deny policy gating for `dom_render`.** The effect is already recorded in
  the (capability-bearing) journal; render output is benign, so a default-allow
  ask/deny knob is a small `RuntimePolicy` config follow-up, deferred to avoid
  perturbing snapshot-manifest serialization here.

## The core observation

The DOM looks like a huge messy API, but it decomposes exactly along the lines
the engine already has determinism policies for (`disabled / seeded / captured /
host`):

| DOM surface | Nature | Where it lives in this prototype |
| --- | --- | --- |
| writes (`createElement`, `appendChild`, `setAttribute`, `textContent`) | output | applied to a Rust virtual tree **and** appended to the `Mutation` stream |
| deterministic reads (`children`, `getAttribute`, `tagName`) | pure | computed from the virtual tree, nothing journaled |
| events (`click`, `input`) | non-deterministic input | delivered via `dispatch_event`, appended to the `EventRecord` stream |
| layout/measurement reads (`getBoundingClientRect`) | captured input | **out of scope** here — the other captured-effect direction |

So the DOM is not a new kind of thing to the engine. It is: pure virtual-tree
ops (in-engine) + a captured output stream (render) + a captured input stream
(events). The engine's existing taxonomy already covers all three. The
integration is mostly *modeling*, not new machinery.

## What the prototype implements

* A Rust-side **virtual DOM arena** (`Dom`) addressed by sequential node ids, so
  ids are stable and replay-deterministic.
* DOM-shaped JavaScript bindings installed as `document` / `window` globals:
  `createElement`, `createTextNode`, `getElementById`, `querySelector(All)`,
  `getElementsByTagName` / `ClassName`, `body`, `documentElement`; and on nodes:
  `appendChild`, `insertBefore`, `removeChild`, `replaceChild`, `remove`,
  `cloneNode`, `hasChildNodes`; `setAttribute` / `getAttribute` / `hasAttribute`
  / `removeAttribute`; `addEventListener` / `removeEventListener` /
  `dispatchEvent`; `getBoundingClientRect` and the `offset*` / `client*` /
  `scroll*` size reads; plus accessors `textContent`, `id`, `className`,
  `classList` (add/remove/toggle/contains), `tagName`, `nodeName`, `nodeType`,
  `parentNode`, `children`, `childNodes`, `firstChild`, `lastChild`,
  `nextSibling`, `previousSibling`, `innerHTML`, `outerHTML`, and
  `querySelector(All)`.
* A **mutation journal** — every structural/attribute/text change as an ordered,
  serde-serializable `Mutation`. This *is* the render protocol: ship it to a dumb
  renderer, or diff it against a prior run.
* An **event journal** — every delivered event as an ordered `EventRecord`, with
  full W3C dispatch (capture → target → bubble, `stopPropagation`,
  `stopImmediatePropagation`, `preventDefault`/`defaultPrevented`, `once` and
  `capture` listener options, de-duped registration, `removeEventListener`).
* A **measurement journal** — every layout read as a `MeasureRecord`, addressed
  by `(node, kind, seq)`. In record mode it is queried from a
  [`MeasurementProvider`] (the renderer-side seam) and journaled; in replay mode
  it is served from the journal with no provider. This is the captured-read
  direction, modelled exactly like `fetch`/`crypto`/timers.
* `SessionJournal` — `events + measurements`, the complete non-deterministic
  input log for a run, serde-serializable so a session is persistable JSON.
* `DomHandle`, the embedder-facing seam: `drain_mutations()`, `dispatch_event()`
  / `replay()`, `journal()` / `load_journal_for_replay()`,
  `set_measurement_provider()`, `render_html()`, `element_by_id()`,
  `strong_count()` (lifetime assertion).

## The property that makes it interesting

Events and measurements are the *only* sources of non-determinism, and both are
journaled. Therefore **the same program + the same `SessionJournal` ⇒
byte-identical mutation journal + rendered HTML.** The tests assert exactly this:

* `mutation_stream_is_deterministic` — two independent runs match byte-for-byte.
* `replaying_event_journal_reproduces_state` — record a session, replay only its
  event journal against a fresh document, get the identical mutation journal.
* `prefix_replay_is_a_time_machine` — replaying the first *k* events lands on the
  exact state the live session had after *k* interactions (fork-at-step-*k*).
* `measurements_replay_from_journal_without_a_provider` — a captured layout read
  comes back identical in replay with the renderer entirely absent.

That is the substrate for time-travel debugging of a UI, "record a session,
replay it as a test", and fork-and-edit-rerun of an agent-built interface.

## How it slots into Chidori's durable host

The engine's durability seam is `Engine::install_chidori_effects(dispatch)` in
[`crates/chidori-js/src/lib.rs`](../crates/chidori-js/src/lib.rs): host effects
forward through `dispatch(effect, json) -> json`, and the main crate routes those
through its call-log + journal (record/replay). The DOM joins that seam in three
directions — and
[`tests/dom_host_integration.rs`](../crates/chidori-js/tests/dom_host_integration.rs)
exercises it: an agent calls `chidori.prompt()` to decide a UI, the render batch
and the user click both flow through the *same* dispatcher, and the resulting log
is one causal sequence `prompt → dom_render → dom_event → log`.

1. **Output (render) as a captured effect.** Flush a `drain_mutations()` batch
   through `dispatch("dom_render", batch)`. On replay it is a no-op served from
   the journal; live, it is handed to the renderer. Mutations are already the
   serde shape the dispatch boundary wants.
2. **Input (events) as a captured host input.** An inbound UI event becomes a
   recorded host result, exactly like `chidori.input()` today: in record mode the
   real event is journaled and delivered via `dispatch_event`; on replay the
   journaled event is re-delivered, reproducing the mutation stream.
3. **Measurement (layout) as a captured read.** `getBoundingClientRect` & the
   `offset*`/`client*` reads route through the `MeasurementProvider` in record
   mode (journaled) and the journal in replay mode — the same record/replay
   contract, for the one DOM read that genuinely depends on a renderer.

Combined with the existing branch / edit-and-rerun flow
(`src/runtime/host_branch.rs`), an agent iterating on a UI gets: fork at an
interaction, edit the generating code, re-run — with all upstream LLM work
replayed for free and the UI diff falling out of the mutation journal.

## Closed gaps

The earlier sketch's gaps are now closed and tested:

* **Layout/measurement** — implemented as the captured-read journal above
  (`MeasurementProvider`, `MeasureRecord`); record/replay verified end to end.
* **No arena leak** — wrapper closures hold a `Weak` back-reference; the embedder
  `DomHandle` holds the sole strong `Rc`, so the VM/realm never keeps the document
  alive and it drops deterministically when the handle drops. The
  `document_arena_is_not_leaked_into_a_cycle` test asserts `strong_count() == 1`
  after building 25 listener-bearing nodes referenced from JS. (Trade-off: if the
  embedder drops the handle while JS still holds `document`, DOM calls degrade to
  graceful no-ops returning `undefined` rather than touching freed state.)
* **Full event model** — capture/target/bubble phases, propagation control,
  `preventDefault`, `once`/`capture` options, `removeEventListener`, de-duped
  registration, and JS-side `dispatchEvent`.

## Remaining limitation

* **Engine performance.** A from-scratch RC-GC interpreter is well below V8 —
  fine for agent-driven iteration and server-authoritative diffing, not aimed at
  60fps client-side production apps. This is inherent to the engine, not the DOM
  layer. (`innerHTML`/`outerHTML` are getters only — there is no HTML *parser*;
  build trees via the DOM API, not by assigning markup strings.)

## Running real React on the runtime

Because the engine is a real JavaScript engine and the DOM is real enough,
**React 18 + `react-dom/server` execute unmodified on chidori-js** — function
components, composition, props, and hooks (`useState` for the initial render).
Its output mounts into the journaled DOM via `el.innerHTML = …` (backed by a
small HTML parser for the well-formed markup server renderers emit), so the
agent can test the result with ordinary DOM queries. See
[`tests/react_ssr.rs`](../crates/chidori-js/tests/react_ssr.rs).

That unlocks the killer loop — an agent iterating on a React component, gated by
a test suite, with fork + replay (`examples/react_agent_demo.rs`, media in
`docs/media/react-agent*.svg`):

1. The agent drafts a component; it is server-rendered, mounted into the DOM, and
   tested. Failing tests drive the next revision until the suite is green.
2. **Fork → edit → replay.** A second engine replays the agent's recorded drafts
   for *free* (0 new model calls) to reconstruct the green state, then makes one
   fresh call for the edit (a dark, annual variant) — shown side by side.
3. **Record == replay.** Re-running the journal yields byte-identical output.

The point: every iteration is deterministic, testable, forkable, and cheap —
the expensive model calls replay instead of re-running.

## The artifact, in one line

A forkable, time-traveling, fully-journaled UI session where the agent and the
interface share one causal log — Phoenix-LiveView-style server-authoritative UI
with Replay.io-style determinism and a branch model neither has.
