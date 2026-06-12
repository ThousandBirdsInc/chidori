# Branching — fork an agent into strategies, compare, pick one

A runnable example of **`chidori.branch`** (see
[`docs/branching-execution.md`](../../docs/branching-execution.md)): an agent
forks itself mid-run into N branches that each explore a strategy **from the
same anchored state**, then compares the outcomes and picks one.

The agent ([`agent.ts`](./agent.ts)) does shared "research" once, then forks:

```ts
const outcomes = await chidori.branch([
  { label: "outline-first", source: "examples/branching/strategies/outline_first.ts", input: { topic, research } },
  { label: "draft-direct",  source: "examples/branching/strategies/draft_direct.ts",  input: { topic, research } },
]);
const best = outcomes.filter((o) => o.status === "completed").reduce(pick);
```

Why this beats re-running the whole agent per idea:

- **The prefix is paid once.** Branches act on the prefix's *result* (the
  parent's VFS plus the explicit `input`); they don't re-derive it — so the
  shared state is byte-identical across branches and the only variable is each
  branch's code. A controlled experiment, not a stochastic re-roll.
- **Each branch is its own editable source.** Edit
  `strategies/outline_first.ts` and re-run: the branch re-anchors to the same
  shared state, the other strategy is untouched.
- **One durable record.** The fan-out is recorded as a single `branch` call
  whose result is the outcomes array. `chidori replay <run-id>` returns it
  from the call log without re-running either branch.
- **Branches nest in the trace.** Each branch's host calls carry
  `parent_seq = the branch call's seq`, so an OTLP viewer (tael) shows a
  `branch` span with one subtree per strategy, side by side.

## Run it

```bash
cargo run -- run examples/branching/agent.ts --input '{"topic": "incident postmortem"}'
```

Branch `source` paths resolve like `callAgent` paths (relative to the working
directory), so run from the repository root.

With an OTLP endpoint configured the fork is visible live:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 \
  cargo run -- run examples/branching/agent.ts
```

The strategies are offline-friendly local transforms; swap their bodies for
`chidori.prompt(...)` calls to compare real model strategies (each branch's
LLM spend is live — cap the fan-out accordingly).

## Outcome shape

```ts
type BranchOutcome = {
  label: string;
  branchId: string;              // <parent run id>-branch-<k>
  status: "completed" | "paused" | "failed";
  output?: Json;                 // when completed
  pendingPrompt?: string;        // when paused (e.g. a chidori.input prompt)
  error?: string;                // when failed
};
```

A `paused` branch is reported (Phase 1) — resuming it out-of-band is the
Phase 2 follow-up in the design doc. Nested `chidori.branch` inside a branch
is rejected.
