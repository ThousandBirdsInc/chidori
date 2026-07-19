---
title: "Harness Engineering Needs a Substrate"
description: "Why harness engineering needs a durable execution substrate \u2014 a response to Lilian Weng's essay."
---

# Harness Engineering Needs a Substrate

*A response to Lilian Weng's ["Harness Engineering for
Self-Improvement"](https://lilianweng.github.io/posts/2026-07-04-harness/)
(2026-07-04), with a runnable demo.*

Weng's argument, compressed: near-term recursive self-improvement won't come
from models rewriting their own weights. It runs through the **harness** — the
system around the model that orchestrates execution, manages context, stores
artifacts, and evaluates results. Her "self-harness loop" has three stages:

1. **Weakness mining** — cluster failures into verifier-grounded patterns
2. **Harness proposal** — bounded edits addressing root causes
3. **Validation** — regression tests on held-in/held-out splits

We think this is right. We'd add one thing the essay leaves implicit: every
stage of that loop makes an infrastructure demand that today's agent stacks
mostly can't meet. **The loop needs a substrate.**

## What each stage actually requires

**Weakness mining** requires that failures be *evidence*, not anecdotes. You
need the full trajectory — every prompt, tool call, token count, error — stored
queryably, clusterable by failure mode, trendable over time. Most stacks have
logs. Mining needs telemetry with an opinion about agents.

**Harness proposal** requires that a proposed fix be testable as a *controlled
experiment*. If your revised strategy runs from a fresh start, you changed two
things: the strategy and the state. To isolate the variable, you must fork
from the failure's exact anchor state — same context, same prior tool results,
same everything — and vary only the edit.

**Validation** requires regression tests that don't lie and don't cost. An
LLM-judged re-run is a sample from a distribution, not a regression test. And
if every case in your suite re-bills the model, your suite stops growing at
exactly the moment it should be compounding.

## The substrate we've been building

We build two tools that turn out to map onto the loop one-to-one — not because
we planned for Weng's essay, but because durability keeps paying dividends in
places we didn't expect:

| Weng's stage | Surface |
|---|---|
| Weakness mining | [**tael**](https://github.com/ThousandBirds/app-tael): OTLP-native traces, issues + failure modes, signal trends, full-text search over LLM payloads |
| Harness proposal | [**Chidori**](https://github.com/ThousandBirds/chidori): `chidori.branch` — fork a run into per-strategy variants from an anchored state; edit-and-rerun; durable workspace |
| Validation | tael eval suites + Chidori checkpoints: golden cases that replay byte-for-byte at $0 |

The seam between them is one attribute: every span a Chidori run emits carries
`chidori.run_id`. A tael trace is a pointer to a replayable run. The round
trip is two commands:

```bash
tael get trace <id>            # ... Chidori run: 2846360f-… / Replay ($0): chidori resume …
chidori resume agent.ts 2846360f-… --ci
```

## The claim that matters: the golden case IS the failed run

Here's the part nobody else can offer, because nobody else records runs
deterministically.

In every eval framework we know of, promoting a production failure to a test
case means *describing* it: the input, the expected behavior, maybe the trace
attached as context. The case is a story about the run.

In Chidori, every run is a checkpoint — the complete, replayable record of
every side effect. So when tael promotes a failing trace to a golden case
(`tael eval case add --from-trace`), it captures the checkpoint reference, and
the case's fixture **is the failed run itself**:

- **Replay it** — `chidori resume <agent.ts> <run-id> --ci` re-executes the
  agent against the recorded log: byte-identical, zero LLM spend,
  milliseconds. Exit 0 means the behavior is exactly preserved. Exit 3 means
  drift — with the first mismatching call in a machine-readable report. Strict
  mode compares the *arguments the agent passes now* against what was
  recorded, so a changed prompt fails loudly instead of silently returning
  stale cached results.
- **Fork it** — `chidori branch-rerun` re-runs a strategy fresh from the
  failure's anchor state. The controlled experiment Weng's stage 2 demands is
  a first-class operation, not a harness you build.
- **Commit it** — `chidori checkpoint export` produces a tarball; check it
  into git and CI replays it forever at $0. Your regression suite compounds
  instead of billing.

A regression suite whose cases cost nothing to run changes the economics of
the whole loop. Weng notes that self-improvement is bottlenecked by evaluation;
$0 deterministic replay removes the marginal cost of *never regressing*, which
is the half of evaluation you can actually make free.

## The loop, closed and runnable

We shipped a demo that walks all three stages end to end on a laptop, no API
key required:
[`examples/self-harness-loop/`](https://github.com/ThousandBirds/chidori/tree/main/examples/self-harness-loop).

1. A worker agent fails on a flaky tool; the trace (with error spans) streams
   to tael, the checkpoint persists.
2. `tael issue create` classifies it; `tael eval case add` promotes it — the
   case's fixture is the checkpoint.
3. A **reflector agent** (a Chidori agent, ~100 lines) pulls the failed
   trajectory from tael's API, prompts for a diagnosis, and writes a revised
   strategy into the durable workspace.
4. `chidori.branch` forks the incumbent and the proposal from one anchored
   prefix. `tael experiment compare <run-id>` scores the A/B per variant — the
   branch labels ride on the spans, no instrumentation.
5. `chidori resume --ci` locks the winner in as a $0 regression test;
   `tael signal trend tool_error` watches the failure mode's frequency drop.

Every step is a shipped command. The only custom code is the reflector — and
the failing tool.

## What we're not claiming

**We supply the loop, not the mind.** Proposal quality — knowing *what* harness
edit addresses the root cause — is the frontier model's job. Our demo reflector
writes a retry strategy from a template; with a real model behind it, the
proposal is the model's. The substrate's guarantee is everything around it:
evidence, controlled variables, deterministic validation, free regression.

**Weak evaluators are everyone's problem.** Tael scores what your scorers
emit. Byte-identical replay verifies *behavior preserved*, which is the
regression half of validation; it does not judge whether new behavior is
*better* — that still needs semantic evals with all the care Weng describes
(held-out splits, resistance to reward hacking). Her caveats are ours.

**The demo failure is seeded.** Real weakness mining is messier than one flaky
tool. The point of the demo is the plumbing: that a failure becomes evidence,
an edit becomes an experiment, a fix becomes a free regression test — with no
glue code.

## Where this goes

The next layer up is making the call log a *dataset*, not just a replay
mechanism: a checkpoint-reading API to diff two trajectories; a cross-run
skill library that a curator agent maintains (tael's comments and issues
already hold the insights — injection at run-start is the missing half);
branch archives with lineage and scores for evolutionary-scale search; eval
splits as a first-class concept, so held-out hygiene is enforced by the tool
rather than by discipline.

But the core loop doesn't wait for any of that. If your agent runs are durable
and your telemetry knows what an agent is, self-improvement infrastructure
stops being a research artifact and starts being two CLIs and an env var.

---

*Chidori: [github.com/ThousandBirds/chidori](https://github.com/ThousandBirds/chidori)
· tael: [github.com/ThousandBirds/app-tael](https://github.com/ThousandBirds/app-tael)
· The demo: [`examples/self-harness-loop/`](https://github.com/ThousandBirds/chidori/tree/main/examples/self-harness-loop)*
