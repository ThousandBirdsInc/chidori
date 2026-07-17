# Record & Replay — agent patterns on non-LLM behaviors

These examples demonstrate Chidori's **record-and-replay durable execution** on
simple, deterministic, *non-LLM* behaviors — so the durability mechanics are the
only thing on screen. Every pattern here is exactly what you need when the
behavior under the hood *is* an LLM call: take an action once, pause for a human,
reproduce a run, retry a flaky dependency, resume after an edit.

Durability in Chidori is **deterministic replay of an effect journal**, not a
frozen VM image. The first run records the result of each *host effect* (a tool
call, an `input()`, a timer, a random id) into a small journal / call log.
Replay re-runs your code from the top and feeds each effect its recorded result
instead of performing it again — so external actions happen **exactly once**, and
a run is **perfectly reproducible**. Because the code re-runs (rather than a
program counter being restored), you can even **edit the code after the point it
paused** and resume cleanly.

The same idea is shown at two layers:

| Layer | Where | Run without a server? | The primitive |
|-------|-------|-----------------------|---------------|
| **`chidori-js` engine (Rust)** | `crates/chidori-js/examples/*.rs` | ✅ yes | `ReplayRuntime::record` / `restore` / `drive` |
| **TypeScript SDK (`@1kbirds/chidori`)** | this directory | needs `chidori run`/`serve` | `defineTool` / `step` / `input` / `memory` + `AgentClient.replay` |

---

## Running the examples

All commands are run **from the repository root**.

### Prerequisites

- A Rust toolchain (`cargo`) — for both layers.
- Node.js ≥ 18 — only for the SDK driver (`driver.mjs`).
- Build the CLI once (needed for everything in Layer 2):

  ```bash
  cargo build --release          # produces ./target/release/chidori
  ```

- Build the TypeScript SDK once (needed only for the SDK driver in step 3 —
  `driver.mjs` imports the built `sdk/typescript/dist/`):

  ```bash
  npm --prefix sdk/typescript install
  npm --prefix sdk/typescript run build
  ```

> **Why `--trusted` below:** `chidori run` is **ask-by-default** for gated
> effects (network, workspace writes) and `chidori serve` is
> **deny-by-default**. These agents are in-repo code you're running on
> yourself, so the commands pass `--trusted` for the permissive posture. (The
> offline agents here only `log`/`memory`/`input`, none of which are gated, so
> they run the same either way — the flag is for consistency.) See
> [`docs/running-modes.md`](../../docs/running-modes.md).

### 1. Layer 1 — run the Rust engine examples (no server, fastest)

Run one:

```bash
cargo run -p chidori-js --example exactly_once
```

Run all six (each prints its journal, replays, and asserts the result):

```bash
for ex in exactly_once human_approval deterministic_identity \
          retry_flaky_tool durable_step edit_and_resume; do
  echo "== $ex =="
  cargo run -q -p chidori-js --example "$ex"
done
```

### 2. Layer 2 (CLI) — record → trace → replay a TS agent

`chidori run` writes a run under `examples/record-replay/.chidori/runs/<id>/`;
`resume` replays it. Capture the newest run id automatically so you don't have
to copy it by hand:

```bash
BIN=./target/release/chidori

# record (--trusted: the agent's tool calls run without per-call prompts)
$BIN run examples/record-replay/exactly_once.ts -i name=Ada --trusted

# grab the id of the run just created
RUN_ID=$(ls -t examples/record-replay/.chidori/runs | head -1)

# inspect the recorded call log (each tool's host calls, journaled inline)
$BIN trace "$RUN_ID" -d examples/record-replay

# replay — the tools' side-effect host calls are served from the log, not re-run
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay
```

Swap `exactly_once.ts` for any agent in this directory
(`deterministic_identity.ts`, `retry_flaky_tool.ts`, `tool_pipeline.ts`).
`human_approval.ts` pauses for input — drive that one with the SDK (step 3).

### 3. Layer 2 (SDK) — drive a server with `AgentClient`

Two terminals: one serves a single agent, the other records + replays it.

```bash
# terminal 1 — serve one agent (--trusted: allow its tool calls)
cargo run -- serve examples/record-replay/exactly_once.ts --port 8080 --trusted

# terminal 2 — run, checkpoint, replay; assert the output is byte-identical
node examples/record-replay/driver.mjs --scenario exactly_once
```

Point the driver elsewhere with `--url`, or override the input with
`--input '{"name":"Grace"}'`. Scenarios: `exactly_once`,
`deterministic_identity`, `retry_flaky_tool`, `tool_pipeline`, `human_approval`
(the last demonstrates pause → resume).

### (optional) Typecheck the agents against the SDK types

```bash
npx -y -p typescript@5.4 tsc -p examples/record-replay
```

The detailed walkthroughs for each layer — including the modify-and-resume and
"prove exactly-once" tricks — are in the sections below.

---

## Scenarios

Each scenario is implemented in **both** layers — a runnable Rust example that
needs no server, and a TS agent you drive with the CLI or the SDK.

| Scenario | Agent capability it gives you | Rust example | TS agent |
|----------|-------------------------------|--------------|----------|
| **Exactly-once side effects** | An action (charge, email, provision) fires once, never on replay | `exactly_once.rs` | `exactly_once.ts` |
| **Durable pause / resume** | Wait for a human/webhook across process restarts | `human_approval.rs` | `human_approval.ts` |
| **Deterministic identity** | Reproducible ids, clocks, sampled choices for audit/debug | `deterministic_identity.rs` | `deterministic_identity.ts` |
| **Resilient retries** | Replay reproduces the exact failure→success path | `retry_flaky_tool.rs` | `retry_flaky_tool.ts` |
| **Memoized expensive work** | Expensive deterministic steps run once, not on every resume | `durable_step.rs` | _(engine-level primitive)_ |
| **Modify-and-resume** | Fix downstream logic and resume without redoing prior work | `edit_and_resume.rs` | _(see "Edit then resume" below)_ |

---

## Layer 1 — the `chidori-js` engine (Rust, no server)

Each example records a tiny JS "agent" against host effects, prints the journal,
then replays from it and asserts the result is reproduced with **zero** live
effect calls. They are self-contained and fast:

```bash
cargo run -p chidori-js --example exactly_once
cargo run -p chidori-js --example human_approval
cargo run -p chidori-js --example deterministic_identity
cargo run -p chidori-js --example retry_flaky_tool
cargo run -p chidori-js --example durable_step
cargo run -p chidori-js --example edit_and_resume
```

Read these first if you want to see the mechanism with nothing else around it —
the journal is printed inline, and the replay handler `panic!`s if any effect is
re-invoked, so the "served from the journal" guarantee is enforced by the code.

---

## Layer 2 — the TypeScript SDK

Build the CLI once:

```bash
cargo build --release
```

### Option A — the CLI (`run` → `trace` → `resume`)

`chidori run` records a run under `.chidori/runs/<id>/`. `chidori resume` replays
it: tool calls return their recorded results instead of executing.

```bash
# record
cargo run -- run examples/record-replay/exactly_once.ts -i name=Ada --trusted
# inspect the recorded call log
cargo run -- trace <run-id> -d examples/record-replay
# replay — the tools' side-effect host calls are served from the log
cargo run -- resume examples/record-replay/exactly_once.ts <run-id> -d examples/record-replay
```

(`resume` needs no `--trusted`: every recorded host call is served from the
call log, so no gated effect ever re-executes.)

**See exactly-once for yourself:** record a run and look at its trace, then
resume and look again. Each side effect (`open_ticket (SIDE EFFECT)`,
`send_email (SIDE EFFECT)`) is a `chidori.log` host call that appears **once**
in the journal; on resume those host calls are served from the log — not
re-emitted — so the ticket is filed and the email sent exactly once no matter
how many times you replay:

```bash
cargo run -- run examples/record-replay/exactly_once.ts -i name=Ada --trusted
RUN_ID=$(ls -t examples/record-replay/.chidori/runs | head -1)
cargo run -- trace "$RUN_ID" -d examples/record-replay      # one SIDE EFFECT log each
cargo run -- resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay
# -> returns the recorded { delivered: true, ... }; no new SIDE EFFECT lines
```

The tool bodies run in the agent's own VM, so their wrapper code re-runs on
replay — but the *host calls* inside them (the side effects) are served from
the journal, which is what makes the effect happen exactly once.

### Option B — the SDK (`AgentClient`, over HTTP)

This mirrors how you'd use the published `@1kbirds/chidori` npm package
(`import { AgentClient } from "@1kbirds/chidori"`). Start a server for one agent, then run
the matching driver scenario:

```bash
# terminal 1 — serve one agent (--trusted: allow its tool calls)
cargo run -- serve examples/record-replay/exactly_once.ts --port 8080 --trusted

# terminal 2 — record, checkpoint, replay; assert the output is identical
node examples/record-replay/driver.mjs --scenario exactly_once
```

For the human-in-the-loop agent, the driver demonstrates **pause → resume**:

```bash
cargo run -- serve examples/record-replay/human_approval.ts --port 8080 --trusted
node examples/record-replay/driver.mjs --scenario human_approval
#   run -> status "paused"
#   client.resume(id, "approve") -> status "completed", refund issued
#   replay of the approved run -> no re-prompt, identical output
```

Scenarios: `exactly_once`, `deterministic_identity`, `retry_flaky_tool`,
`tool_pipeline`, `human_approval`.

### Edit then resume (modify-and-resume)

Record any run, edit the agent's logic *after* the point it paused/finished
reading effects (e.g. change how `tool_pipeline` formats its briefing), and
resume with `--allow-source-change`: the recorded tool calls are reused and
only the new tail runs. The flag is required because resume verifies the
agent source against the run's snapshot manifest and refuses on mismatch —
cached results are never silently paired with changed code:

```bash
cargo run -- resume examples/record-replay/tool_pipeline.ts <run-id> \
  -d examples/record-replay --allow-source-change
```

Editing an *already-executed* step instead is rejected with a clear divergence
error (the `edit_and_resume.rs` example shows both halves).

---

## Authoring rules these examples follow

A few constraints make agents and tools replay cleanly — worth knowing before you
write your own:

- **Route side effects through host calls.** A `defineTool` body runs in the
  agent's own VM, so on replay the body re-runs but its host calls
  (`chidori.log` / `chidori.fetch` / `chidori.memory`) are served from the
  journal. Put the real side effect (the API POST) in a host call — that's what
  makes it happen exactly once. Pure compute in the body is deterministic (fixed
  clock, seeded RNG) and simply recomputes identically.
- **Signal tool failure with a return flag, not a thrown error.** The call log
  records a tool's *return value*, so `return { ok: false, status: 503 }` replays
  exactly; a thrown/rejected effect does not currently re-reject cleanly on
  replay. See `retry_flaky_tool.ts`.
- **`workspace.write` needs a root.** Set `CHIDORI_WORKSPACE_ROOT=<dir>` to write
  real file artifacts; otherwise persist durable state with `chidori.memory`
  (what `tool_pipeline.ts` does).
- **Determinism is the default.** The runtime's default policy is
  `date: "fixed"`, `random: "seeded"`, so `Date.now()` and `Math.random()` are
  already reproducible (that's why `deterministic_identity` reports
  `startedAt: 0`). Recording the value through a tool/effect additionally pins it
  across code edits.

## Files

```
exactly_once.ts            human_approval.ts        deterministic_identity.ts
retry_flaky_tool.ts        tool_pipeline.ts
tools.ts                   # offline stand-in tools (defineTool handles), imported by the agents
driver.mjs                 # AgentClient run/checkpoint/replay + pause/resume
tsconfig.json              # typecheck the agents against the in-repo SDK
```
