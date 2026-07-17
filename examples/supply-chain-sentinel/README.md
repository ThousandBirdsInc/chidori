# Supply-Chain Sentinel

A consumer-built demo agent from
[consumer usability review, round 4](../../docs/consumer-usability-review-4.md):
a dependency auditor for this repository's own Rust manifest, built to
exercise Chidori's **day-2 surface** — npm packages without Node, the durable
run store, machine-loss hydration, `--until-seq` time travel, and
`chidori verify` as a CI gate.

## What it does

1. Parses `data/Cargo.toml` + `data/Cargo.lock` with
   [`fast-toml`](https://www.npmjs.com/package/fast-toml) (installed via
   `chidori add`, no Node involved) inside a `chidori.step` value checkpoint.
   (`fast-toml` is the *third* TOML parser tried — `smol-toml` and `confbox`
   both install cleanly but fail at import time; see the review's Finding 3.)
2. Triages each direct dependency with a DeepSeek tool loop over two live
   tools: crates.io metadata and the OSV vulnerability database.
3. Validates every model verdict with [`valibot`](https://valibot.dev) before
   trusting it. (Not zod — the docs' own flagship package example throws
   `ReferenceError: Cannot access binding before initialization` at import
   evaluation, in both v3 and v4; see the review's Finding 4.)
4. Gates publication on a human decision (`chidori.input`).
5. Publishes `AUDIT.md` to the workspace.

## Setup

```bash
cd examples/supply-chain-sentinel
chidori install                      # packages from the checked-in lockfile

export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...
```

## Run it

```bash
# Scripted (deterministic — good for recorded runs):
chidori run agent.ts --trusted --model deepseek-v4-flash \
  --input '{"decision": "publish"}'

# Interactive: answer the publish gate at the terminal.
chidori run agent.ts --trusted --model deepseek-v4-flash

# With a durable mirror, so the run survives losing this machine:
CHIDORI_RUN_STORE=sqlite CHIDORI_DURABILITY=strict \
  chidori run agent.ts --trusted --model deepseek-v4-flash
```

## Day-2 operations this demo exercises

```bash
# Free, byte-identical replay (no provider needed):
chidori resume agent.ts <run_id>

# Machine loss: delete the local run dir, hydrate it back from the mirror:
rm -rf .chidori/runs/<run_id>
CHIDORI_RUN_STORE=sqlite chidori resume agent.ts <run_id> --trusted

# Time travel: replay records 1..N for free, continue live from there —
# e.g. re-answer the publish gate without re-paying the audit:
chidori resume agent.ts <run_id> --until-seq <seq> --trusted

# CI: assert the checked-in run still replays byte-identically:
chidori verify agent.ts <run_id>
```

See the round-4 review for how each of these behaved in practice.
