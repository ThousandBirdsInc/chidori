# DOM behind the host boundary — a prototype

This is a working prototype of an idea: put a DOM behind the *same journaled
host boundary* that Chidori already uses for `prompt`, `tool`, and `fetch`, and
you get a UI runtime where the agent's reasoning, its LLM/tool calls, the
resulting DOM mutations, and the user's interactions all live in one
deterministic, replayable journal.

It lives in [`crates/chidori-js/src/dom.rs`](../crates/chidori-js/src/dom.rs),
with tests in
[`crates/chidori-js/tests/dom_prototype.rs`](../crates/chidori-js/tests/dom_prototype.rs)
and a runnable demo in
[`crates/chidori-js/examples/dom_session.rs`](../crates/chidori-js/examples/dom_session.rs)
(`cargo run -p chidori-js --example dom_session`).

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
  `createElement`, `createTextNode`, `getElementById`, `body`,
  `documentElement`; and on nodes: `appendChild`, `insertBefore`, `removeChild`,
  `remove`, `setAttribute` / `getAttribute` / `removeAttribute`,
  `addEventListener`, plus `textContent`, `id`, `className`, `tagName`,
  `parentNode`, `childNodes` accessors.
* A **mutation journal** — every structural/attribute/text change as an ordered,
  serde-serializable `Mutation`. This *is* the render protocol: ship it to a dumb
  renderer, or diff it against a prior run.
* An **event journal** — every delivered event as an ordered `EventRecord`.
* `DomHandle`, the embedder-facing seam: `drain_mutations()` (incremental render
  batch), `dispatch_event()` / `replay_events()`, `render_html()`,
  `element_by_id()`.

## The property that makes it interesting

Events are the *only* source of non-determinism, and they are journaled.
Therefore **the same program + the same event journal ⇒ byte-identical mutation
journal + rendered HTML.** The tests assert exactly this:

* `mutation_stream_is_deterministic` — two independent runs match byte-for-byte.
* `replaying_event_journal_reproduces_state` — record a session, replay only its
  event journal against a fresh document, get the identical mutation journal.
* `prefix_replay_is_a_time_machine` — replaying the first *k* events lands on the
  exact state the live session had after *k* interactions (fork-at-step-*k*).

That is the substrate for time-travel debugging of a UI, "record a session,
replay it as a test", and fork-and-edit-rerun of an agent-built interface.

## How it slots into Chidori's durable host

Today the engine's durability seam is `Engine::install_chidori_effects(dispatch)`
in [`crates/chidori-js/src/lib.rs`](../crates/chidori-js/src/lib.rs): host effects
forward through `dispatch(effect, json) -> json`, and the main crate routes those
through its call-log + journal (record/replay). The DOM joins that seam in two
directions:

1. **Output (render) as a captured effect.** Flush a `drain_mutations()` batch
   through `dispatch("dom_render", batch)`. On replay it is a no-op served from
   the journal; live, it is handed to the renderer. Mutations are already the
   serde shape the dispatch boundary wants.
2. **Input (events) as a captured host input.** An inbound UI event becomes a
   recorded host result, exactly like `chidori.input()` today: in record mode the
   real event is journaled and delivered via `dispatch_event`; on replay the
   journaled event is re-delivered, reproducing the mutation stream. This reuses
   the existing pending/resume machinery — a UI event is just another thing the
   agent durably waits on.

Combined with the existing branch / edit-and-rerun flow
(`src/runtime/host_branch.rs`), an agent iterating on a UI gets: fork at an
interaction, edit the generating code, re-run — with all upstream LLM work
replayed for free and the UI diff falling out of the mutation journal.

## Known gaps (prototype, honestly scoped)

* **No layout/CSS/measurement.** `getBoundingClientRect` & friends are the other
  captured-input direction and need a real renderer in the loop during record;
  not implemented here.
* **Wrapper cache leaks the arena.** Node wrappers are cached on the node for
  stable JS identity (`el.parentNode === container` holds). That cache forms an
  `Rc` cycle with the native closures it holds, so the document is freed at
  session end, not incrementally. A production version would hold wrappers via
  GC-traced host slots (the engine has a cycle collector in `gc.rs`; wiring DOM
  nodes into it is the clean fix).
* **Event model is minimal.** Dispatch bubbles target→root; no capture phase, no
  `stopPropagation`/`stopImmediatePropagation`, no default actions.
* **Engine performance.** A from-scratch RC-GC interpreter is well below V8 —
  fine for agent-driven iteration and server-authoritative diffing, not aimed at
  60fps client-side production apps.

## The artifact, in one line

A forkable, time-traveling, fully-journaled UI session where the agent and the
interface share one causal log — Phoenix-LiveView-style server-authoritative UI
with Replay.io-style determinism and a branch model neither has.
