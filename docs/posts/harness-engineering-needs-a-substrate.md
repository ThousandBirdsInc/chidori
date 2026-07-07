# Harness Engineering Needs a Substrate

*A response to Lilian Weng's ["Harness Engineering for Self-Improvement"](https://lilianweng.github.io/posts/2026-07-04-harness/), from the team building Chidori and Tael.*

---

Last week, Lilian Weng published the clearest articulation yet of where
near-term self-improvement in AI systems actually comes from. Not from models
rewriting their own weights — from **harness engineering**: evolving the
system *around* the model that "orchestrates execution and decides how the
model thinks and plans, calls tools and acts, perceives and manages context,
stores artifacts, and evaluates results."

We think she's right. And we think there's a consequence buried in the essay
that deserves to be said out loud:

**Every self-improvement loop she describes is an experiment loop. And
experiment loops are an infrastructure problem before they are an
intelligence problem.**

Look at the systems she surveys — ADAS searching over agent designs, AFlow
optimizing workflow graphs, the Darwin Gödel Machine editing its own harness
code, AlphaEvolve breeding candidate programs. Strip the names away and every
one of them runs the same cycle: *generate a variant → run it → evaluate it →
keep or discard → repeat.* Which means every one of them has the same
unstated dependencies:

- **Controlled variables.** If you change the harness *and* the stochastic
  prefix changes underneath you, you can't tell whether your edit helped or
  the randomness moved.
- **Recorded trajectories.** You can't mine weaknesses from runs you didn't
  record.
- **Cheap re-evaluation.** If every validation re-bills every token, your
  search budget is your API bill.
- **Regression protection.** A loop that can't tell when it got worse will
  confidently get worse.
- **An evaluator outside the loop.** Weng is blunt about this one:
  self-improvement optimizes whatever signal you give it, so the signal has
  to live somewhere the optimizer can't touch, with held-out tests and audit
  trails.

Most agent frameworks provide none of these. They give you the loop's
*syntax* — plan, act, observe — and leave its *laboratory* as an exercise for
the reader.

We've spent the last two years building the laboratory. Two products, and
they turn out to be the two halves of Weng's loop.

## The substrate: every run is an experiment

[Chidori](https://github.com/ThousandBirdsInc/chidori) is an agent framework
with one architectural commitment: **every side effect — every LLM call, tool
call, HTTP request — flows through the runtime as a recorded host call.**
Agents are plain async TypeScript; the runtime sees everything they do.

That one boundary yields, almost for free, the experiment infrastructure
above:

- **Trajectories are durable artifacts by default.** Every run persists as a
  replayable call log — not a log *about* the run; the run itself.
- **Replay costs zero tokens and is byte-identical.** Determinism is enforced
  by runtime policy (fixed clock, seeded randomness), so re-running a
  recorded run isn't an approximation — it's the same run, in milliseconds,
  for $0.
- **`chidori.branch` is a controlled experiment as a primitive.** Fork a run
  mid-flight into N strategies from the *same anchored state*. The shared
  prefix isn't re-run (and can't drift, because it's recorded) — the only
  variable is each branch's code. Edit a losing strategy and re-run *just
  that branch* from the same anchor. This is the inner move of ADAS, AFlow,
  and DGM — evaluate a harness variant while holding everything else
  constant — and we know of no other runtime that offers it.
- **Failures survive.** Failed and paused branches persist alongside winners,
  immutably. Weng flags "negative results" as a blind spot — models trained
  on successes are bad at abandoning hypotheses. You can't learn from
  failures you threw away.
- **Humans sit at the right altitude.** Pause-to-disk on `input()` and
  signals, policy-gated approvals, a deny-by-default sandbox. If you're going
  to let an agent propose edits to its own harness, containment isn't a
  nice-to-have.

## The feedback plane: weakness mining as a product

[Tael](https://github.com/ThousandBirdsInc/tael) is observability built for
agents to *use*, not just humans to look at: OpenTelemetry in, structured
JSON out of a CLI-first interface, LLM spans with typed model/token/cost
fields and prompt payloads stored as deduplicated, searchable blobs.

But the part that matters here is what sits on top of the queries — a
**trace-native reliability loop**:

- Classify a recurring failure straight off its trace:
  `tael issue create --from-trace <id> --failure-mode tool_error`.
- Promote it into a golden regression case:
  `tael eval case add --from-trace <id> --suite support-agent`.
- Run, score, and compare suites across code versions:
  `tael eval run … && tael eval compare <run> <baseline>`.
- Track whether a failure mode is actually receding:
  `tael signal trend context_loss`; compare variants over live traffic:
  `tael experiment compare --signal context_loss --last 24h`.

If that sounds familiar, it's because it is Weng's **self-harness loop**,
stage for stage:

| Weng's self-harness loop | Our stack |
|---|---|
| **Weakness mining** — cluster failures into verifier-grounded patterns | Tael: issues, failure modes, signal trends, anomaly detection, payload search |
| **Harness proposal** — bounded edits addressing root causes | Chidori: `workspace.write` a revised strategy, `branch` it against the incumbent from the failure's exact state |
| **Validation** — regression tests on held-in/held-out splits | Tael eval suites + Chidori checkpoints replayed at $0 |

And her ACE framing of context engineering maps the same way: the
**Generator** is a Chidori run; the **Reflector** is an agent querying Tael's
JSON; the **Curator's** durable store is Tael's trace comments — which
already back issues, signals, and case provenance by design.

One more of her requirements falls out of the architecture rather than a
policy: **the evaluator sits outside the optimization loop.** Tael is a
separate plane the agent reports into but doesn't control. Chidori's
deterministic call log is the tamper-evident record of what actually
happened; Tael is the independent scorer and trend-keeper on top. When a loop
starts gaming its metric — and Weng is clear that it will — the full recorded
trajectory is how you catch it.

## The claim nobody else can make

Here's where the combination becomes more than the sum.

Every eval platform can promote a failing trace into a test case. But a trace
is a *description* of a run. When you "re-run" it, you re-prompt a stochastic
model and hope the failure reproduces.

A Chidori checkpoint is not a description. It *is* the run. So in our stack,
the golden case behind a Tael eval isn't a transcript that resembles the
failure —

**it's the failed run itself, replayable byte-for-byte, forever, for $0 — and
forkable from the exact state where it went wrong.**

Regression testing an agent stops being "probably still fixed" and becomes
"provably unchanged." Validating a harness edit stops costing a re-run of the
whole trajectory and starts costing milliseconds. That's the difference
between a self-improvement loop you can afford to run occasionally and one
you can leave on.

## What we're not claiming

Weng ends her essay with the open problems, and they're real for us too.

We supply the loop, not the mind: the quality of *harness proposals* is the
frontier model's job, and no infrastructure fixes a bad hypothesis. Weak
evaluators remain everyone's problem — Tael scores what your scorers emit,
and research taste still has no fast verifier. And population-scale
evolutionary search (AlphaEvolve-sized archives, lineages, hundreds of
candidates) is beyond what `chidori.branch`'s deliberate cost caps are built
for today. We'd rather tell you where the edges are than discover you found
them.

What we are claiming is narrower and, we think, more useful: **the
self-harness loop is no longer a research diagram. It's two tools you can
`curl | sh` today.** Chidori runs the experiments; Tael grounds the feedback;
a checkpoint in git guards the floor.

We're wiring the seam tighter — run-to-trace correlation, checkpoint-backed
golden cases, and a runnable end-to-end demo of the full loop (worker fails →
Tael mines it → a reflector agent proposes a fix → `chidori.branch` runs the
controlled experiment → `tael eval compare` picks the winner → the checkpoint
lands in CI). Watch the repos.

Everyone is writing the loop. We built the lab.

---

*Chidori: [github.com/ThousandBirdsInc/chidori](https://github.com/ThousandBirdsInc/chidori) ·
Tael: [github.com/ThousandBirdsInc/tael](https://github.com/ThousandBirdsInc/tael) ·
Talk to us on [Discord](https://discord.gg/CJwKsPSgew).*
