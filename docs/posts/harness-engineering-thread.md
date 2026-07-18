# Social thread draft — harness engineering needs a substrate

*Draft for X/Bluesky. Post 1 is the hook; attach the branch-waterfall GIF
(tael TUI showing a chidori.branch fan-out) to post 4. Not yet published.*

---

**1/**
Lilian Weng's new post argues near-term self-improvement runs through the
*harness* — the system around the model — via a loop: mine weaknesses →
propose harness edits → validate.

We think she's right. And every stage of that loop makes an infrastructure
demand most agent stacks can't meet. 🧵

**2/**
The loop needs a substrate:

- weakness mining → failures as *queryable evidence*, not log lines
- harness proposal → fixes testable as *controlled experiments* (same state, one variable)
- validation → regression tests that don't lie (LLM re-runs are samples) and don't cost

**3/**
Our two tools turn out to map onto the loop one-to-one:

| Weng's stage | Tool |
|---|---|
| weakness mining | tael — agent-native OTLP traces, issues, signal trends |
| harness proposal | chidori — fork a run mid-flight into per-strategy branches |
| validation | eval cases whose fixtures are checkpoints, replayed at $0 |

**4/**
The part nobody else can offer: the golden test case is **not a description of
the failed run — it IS the failed run.**

Chidori records every run deterministically. Promote a failing trace to an
eval case and the fixture is the checkpoint itself:

replay it ($0, ms, byte-identical) · fork it from its exact anchor state ·
commit it to git

[GIF: tael TUI waterfall of a chidori.branch A/B]

**5/**
`chidori resume <run> --ci` = a regression gate:

exit 0 → byte-identical replay
exit 3 → drift, with the first mismatching call in a JSON report

A regression suite that costs $0 per run compounds instead of billing. That
changes the economics of "never regress."

**6/**
We closed the whole loop in a runnable demo — laptop, <10 min, no API key:

1. worker fails on a flaky tool → error trace in tael
2. `tael issue create` + `eval case add` (fixture = checkpoint)
3. a ~100-line reflector agent reads the trajectory, writes a retry strategy
4. `chidori.branch` A/Bs old vs new from one anchored state
5. `tael experiment compare` scores it; `resume --ci` locks it in
6. `tael signal trend tool_error` watches the failure rate drop

**7/**
Honest limits: we supply the loop, not the mind. Proposal quality is the
frontier model's job. And byte-identical replay verifies *behavior preserved* —
semantic "is it better" still needs real evals, with all of Weng's caveats
about weak evaluators and reward hacking.

**8/**
If your agent runs are durable and your telemetry knows what an agent is,
self-improvement infra stops being a research artifact and becomes two CLIs
and an env var.

Post: [link to harness-engineering-needs-a-substrate]
Demo: github.com/ThousandBirds/chidori → examples/self-harness-loop
