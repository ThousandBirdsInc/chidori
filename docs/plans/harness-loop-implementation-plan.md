# Implementation Plan: The Self-Harness Loop (Chidori × Tael)

**Status:** Draft · **Owner:** TBD · **Date:** 2026-07-07

## Context

Lilian Weng's ["Harness Engineering for Self-Improvement"](https://lilianweng.github.io/posts/2026-07-04-harness/)
(2026-07-04) argues that near-term recursive self-improvement runs through the
**harness** — the system around the model that orchestrates execution, manages
context, stores artifacts, and evaluates results. Her "self-harness loop" has
three stages:

1. **Weakness mining** — cluster failures into verifier-grounded patterns
2. **Harness proposal** — bounded edits addressing root causes
3. **Validation** — regression tests on held-in/held-out splits

Our two products map onto this loop one-to-one:

| Weng's stage | Product surface |
|---|---|
| Weakness mining | **Tael**: issues + failure modes, signals + trends, anomalies, LLM-payload full-text search, `diagnose` |
| Harness proposal | **Chidori**: `branch` (controlled experiment from an anchored state), edit-and-rerun, `workspace.write` |
| Validation | **Tael** eval suites / `eval compare` + **Chidori** checkpoint-as-test ($0 replay) |

What's missing is the **seam** between the products (they currently don't
reference each other) and a **flagship demo** that closes the loop. This plan
covers both, plus the positioning work to ride the article's traffic.

The unique claim we're building toward: *a golden test case that is not a
description of a failed run, but the failed run itself — replayable
byte-for-byte at $0, forkable from its exact anchor state.* Nobody else can
offer this, because nobody else records runs deterministically.

---

## Phase 0 — Prove the wire works (days, not weeks)

Chidori already emits OTLP (`OTEL_EXPORTER_OTLP_ENDPOINT`; branch fan-outs
render as span subtrees; prompt spans carry `gen_ai.usage.*` including cache
tokens). Tael ingests OTLP on `:4317`. This phase is verification + docs, not
code.

**Tasks**

- [ ] Smoke-test: `tael serve` + `chidori run examples/agents/worker.ts` with
      `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`; confirm
      `tael query traces` returns the run, `tael get trace` shows the prompt
      spans with `gen_ai.*` attributes, and a `chidori.branch` renders as one
      subtree per variant.
- [ ] Fix any attribute-mapping gaps found (e.g. ensure Chidori stamps
      `gen_ai.request.model`, token counts, and cost in the conventions Tael's
      typed LLM fields expect).
- [ ] **Docs (chidori repo):** "Observing runs with Tael" section — one env
      var, three example `tael` queries against a Chidori run.
- [ ] **Docs (tael repo):** "Tracing Chidori agents" quickstart; cross-link
      both READMEs.

**Acceptance:** a fresh clone of each repo + the two READMEs is enough to see
a Chidori branch fan-out in Tael's TUI waterfall.

---

## Phase 1 — The seam: run ↔ trace correlation

Make a Tael trace and a Chidori run two views of the same object.

**Chidori tasks**

- [ ] Stamp `chidori.run_id` as a resource/span attribute on the run's root
      span (and `chidori.branch_id` + `chidori.branch_label` on branch
      subtrees). Location: `crates/chidori/src/runtime/otel.rs` (`RunSpan`).
- [ ] Stamp `chidori.checkpoint_path` (or enough to derive it:
      run dir relative to `.chidori/runs/`) so tooling can go from a trace to
      the replayable artifact.
- [ ] Stamp the prompt `request_digest` on prompt spans (already in call-log
      args; mirror it to OTEL) — gives Tael's full-text/SQL layer a
      content-addressed join key across runs.

**Tael tasks**

- [ ] Recognize `chidori.run_id` / `chidori.branch_label` as first-class
      filterable attributes (`tael query traces --attribute chidori.run_id=…`
      already works generically; add convenience + surfacing in `get trace`
      output).
- [ ] `tael experiment compare` keyed on `chidori.branch_label`, so a
      `chidori.branch` A/B shows up as an experiment comparison with no extra
      instrumentation.

**Acceptance:** round-trip both directions — from `tael get trace <id>` copy a
run id and `chidori resume <run_id>`; from a Chidori run id, find its trace,
comments, and any issues filed against it.

---

## Phase 2 — Golden case = checkpoint (the killer integration)

Today `tael eval case add --from-trace` promotes a *trace* — a description of
a run. A Chidori *checkpoint* is the run itself. Make the checkpoint the
executable fixture behind the golden case.

**Tael tasks**

- [ ] `tael eval case add --from-trace <id>`: when the trace carries
      `chidori.run_id`, capture the checkpoint reference (path or archived
      copy via content-addressed blob storage) on the case record.
- [ ] Document the `--cmd` recipes for `tael eval run`:
      - regression (exact): `chidori resume <agent.ts> <run_id>` — byte-identical
        replay, $0, milliseconds; any divergence = drift caught.
      - live re-test (semantic): `chidori branch-rerun <run_id> <branch_id>` or a
        fresh `chidori run` seeded from the case input — re-executes against
        the current agent source.
- [ ] `tael eval report` / `compare` surface Chidori cost fields (replay runs
      report $0 — make that visible; it's the pitch).

**Chidori tasks**

- [ ] `chidori resume` non-interactive/CI mode: stable exit codes +
      machine-readable divergence report (what `eval run --cmd` consumes).
- [ ] A `chidori checkpoint export <run_id>` (tarball of the run dir) so a
      case fixture can be archived into Tael / committed to git without
      knowing `.chidori/runs/` layout.

**Acceptance:** a failing production trace becomes a suite case with one
command; `tael eval run --suite …` replays every case at $0 and `eval compare`
against a baseline shows the fix; the whole suite runs in CI.

---

## Phase 3 — Flagship demo: the loop, closed

A runnable example implementing Weng's full loop with only shipped commands
plus one new ~100–150 line "reflector" agent. Lives at
`examples/self-harness-loop/` in the chidori repo (cross-linked from tael).

**The demo script**

1. **Run + observe:** a worker agent (based on `examples/agents/worker.ts`)
   handles tasks; traces stream to Tael.
2. **Weakness mining:** a failure occurs (seeded, e.g. a flaky tool);
   `tael issue create --from-trace … --failure-mode tool_error`;
   `tael eval case add` promotes it, fixture = the checkpoint (Phase 2).
3. **Harness proposal:** the **reflector agent** (a Chidori agent) shells out
   to `tael issue examples` / `tael get trace --format json`, reads the
   trajectory, writes a revised strategy to the workspace
   (`chidori.workspace.write("strategies/retry_with_backoff.ts", …)`).
4. **Controlled experiment:** `chidori.branch([{source: old}, {source: new}])`
   from the failure's anchored state — one variable, same prefix.
5. **Validation:** `tael eval run` over the suite with each variant;
   `tael eval compare <candidate> <baseline>`; winner's checkpoint committed
   as a regression test.
6. **Guard:** `tael signal trend tool_error` shows the failure mode's
   frequency dropping; the checked-in checkpoint prevents regression forever.

**Tasks**

- [ ] Write the demo (agent, reflector, strategies, seeded failure, README
      walkthrough with expected output at each step).
- [ ] Verify the reflector can drive `tael` via the captured `fetch`/tool path
      under Chidori's sandbox policy (Tael's REST API on `:7701` — document
      the policy grant needed). If shell-out is required instead, wrap `tael`
      as a Chidori TS tool.
- [ ] Record the run and check in its checkpoint so *the demo itself* replays
      at $0 for anyone evaluating the stack without API keys
      (`CHIDORI_TEST_LLM_RESPONSE` fallback documented).

**Acceptance:** `README.md` walkthrough executes end-to-end on a laptop in
<10 minutes; the loop demonstrably improves the agent and the improvement is
regression-guarded.

---

## Phase 4 — Positioning & content

- [ ] Publish the response post (see
      `docs/posts/harness-engineering-needs-a-substrate.md`) — timing matters;
      the article is days old.
- [ ] Chidori README: add **"Self-improving agents / harness engineering"** to
      *What You Can Build*, linking the demo. Same mechanism, new story — the
      durability features currently pitched for debugging/cost are the RSI
      experiment substrate.
- [ ] Tael README: add the loop diagram with Chidori labeled as the substrate.
- [ ] Social thread distilling the post (the mapping table + the
      golden-case-is-a-checkpoint claim + demo GIF of the branch waterfall in
      Tael's TUI).

---

## Later / roadmap (announce direction, don't block on it)

- **Checkpoint-reading API** (chidori): load a run, iterate records, diff two
  trajectories — turns the call log from a replay mechanism into a dataset.
  Complements (doesn't replace) mining via Tael.
- **Cross-run skill/context library**: durable store an agent reads at run
  start and a curator agent updates — the ACE pattern. Tael comments/issues
  already hold the *insights*; this is the *injection* half.
- **Evolutionary scale**: branch archive with lineage + scores across runs,
  higher fan-out than 16, nested branches — needed for ADAS/AlphaEvolve-class
  search. Not needed for the self-harness loop.
- **Diversity + held-out hygiene**: eval suite splits (held-in/held-out) as a
  first-class Tael concept; guardrails against optimizing on the test set —
  Weng's reward-hacking prescription, productized.

## Risks & honest limits

- **We supply the loop, not the mind.** Harness *proposal* quality is the
  frontier model's job; our demo reflector will be simple. Don't overclaim
  autonomy — claim infrastructure.
- **Weak evaluators are everyone's problem.** Tael scores what `scores.jsonl`
  provides; taste-like qualities remain hard. Say so in the post — it builds
  credibility and matches Weng's own caveats.
- **Sandbox friction** (Phase 3): the reflector needs network egress to Tael;
  get the policy story crisp or the demo undermines the security pitch.

## Sequencing & effort (rough)

| Phase | Effort | Dependency |
|---|---|---|
| 0 — wire + docs | 1–3 days | none |
| 1 — correlation | ~1 week (both repos) | 0 |
| 2 — checkpoint golden cases | 1–2 weeks | 1 |
| 3 — demo | ~1 week | 1 (can start against 0 with manual glue) |
| 4 — content | 2–3 days | post can ship after 0 with the demo as "coming"; ideally after 3 |

Fastest credible path: ship Phase 0 + the post in week one (with the mapping
table and a teaser of the demo), land Phases 1–3 over the following two to
three weeks, then update the post with the runnable demo.
