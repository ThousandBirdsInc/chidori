---
title: "CLI Reference"
description: "Every public chidori subcommand with flags, defaults, and exit codes — plus the approval postures and the chidori deploy family."
---

# CLI reference

One binary, no runtime dependencies — install with the one-line script in
[Getting Started](./getting-started.md). This page is the complete reference
for every public subcommand: flags, defaults, and exit codes, with a link to
the guide page that covers each area in depth.

## Getting started

### `chidori demo`

Interactive picker over the bundled example agents, run in the trusted
posture. It resolves `examples/agents/*.ts` relative to the repo root, so it
works **only from a repo checkout**. The `hello`, tool-use, and input-pause
demos need no provider key; without one, the others print sign-in guidance
and exit 0. No flags.

### `chidori model-login`

Zero-setup provider sign-in: a browser OAuth flow against OpenRouter. The
key is saved to `~/.chidori/credentials.json` and used automatically
whenever no `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` is configured. No flags.

### `chidori init [dir]`

Scaffold a starter project. Refuses to overwrite existing files.

| Flag | Meaning |
|---|---|
| `-t/--template docs\|chat\|worker` | `docs` scaffolds a chat over the bundled Chidori docs (next step: `chidori chat agent.ts`); `chat` is a conversational assistant; `worker` is an autonomous tool-loop agent (next step: `chidori run agent.ts --input task="…"`). Omit to pick interactively. |

These are *scaffold* templates — project starting points. The unrelated
prompt-template host function `chidori.template` is covered in
[Prompt Templates](./template.md).

### `chidori chat [agent.ts]`

Interactive multi-turn REPL backed by `conversation()` (see
[Core Concepts](./core-concepts.md)). With no file, it chats with the model
directly; with an agent file, it chats through the agent — the file must
accept `{ messages, system?, model?, tools? }` and return `{ transcript }`
(or `{ history }`). Each turn is a durable host call; prior turns replay
from the journal, so only the newest message reaches the provider.

| Flag | Meaning |
|---|---|
| `-s/--system` | System prompt (direct-chat mode). |
| `-m/--model` | Model for the chat. |
| `--resume <session_id>` | Continue a prior chat; earlier turns replay for $0. |
| `--untrusted` / `--trusted` | Posture override (mutually exclusive) — see [Approval postures](#approval-postures). |

### `chidori check <agent.ts>`

Validate an agent file without running it. No flags. Exits **2** on failure
— the only command that does.

## Running

### `chidori run <agent.ts>`

One-shot run, journaled under `.chidori/runs/<run_id>/` next to the agent
file. Walkthrough: [Getting Started](./getting-started.md).

| Flag | Meaning |
|---|---|
| `-i/--input` | Repeatable. `key=value`, a JSON object string, `@file.json` (whole input object), or `key=@path` (value read from a file). |
| `--model` | The run's default model (same as `CHIDORI_MODEL`). |
| `--stream` | NDJSON progress events on stdout (`--trace` is ignored with it). |
| `--trace` | JSON trace to stdout. |
| `-v/--verbose` | Host calls to stderr. |
| `--untrusted` / `--trusted` | Posture override (mutually exclusive). |
| `--isolate` / `--no-isolate` | OS isolation for the agent; `--isolate` is the Unix default (`--no-isolate` = `CHIDORI_ISOLATE=off`). |

### `chidori dev <agent.ts>`

Edit-and-replay loop: records one run, then watches the agent and re-runs on
every save, replaying recorded calls from the journal — so edits cost zero
tokens for everything the recording answers. Flags: `-i/--input`, `--model`,
`--untrusted` / `--trusted`. See [Replay & Resume](./replay.md).

### `chidori serve [agent.ts]`

HTTP session server: sessions, pause/resume, signals, approvals, SSE
streaming. A session is a served run — a session id **is** a run id. With no
agent file it hosts only the detached-agent fleet re-armed from
`.chidori/runs/`, and each session request must name an `agent`. Endpoint
reference: [Running Modes](./running-modes.md); fleet:
[Detached Agents](./detached-agents.md); production checklist:
[Deployment](./deployment.md).

| Flag | Meaning |
|---|---|
| `-p/--port` | Default 8080. |
| `--host` | Default loopback. `--host 0.0.0.0` (or `CHIDORI_HOST`) exposes it; a non-loopback bind requires `CHIDORI_API_KEY` unless `CHIDORI_ALLOW_UNAUTHENTICATED=1` — and the server speaks plain HTTP either way, so front it with TLS. |
| `--app <manifest>` | Boot from an application manifest; `chidori.app.yml`/`.yaml`/`.json` next to the agent is auto-discovered (`CHIDORI_APP_MANIFEST` too). |
| `--model`, `-v/--verbose` | As on `run`. |
| `--untrusted` / `--trusted`, `--isolate` / `--no-isolate` | As on `run`. |

## Packages

| Command | Flags | What it does |
|---|---|---|
| `chidori add <packages…>` | `-D/--dev`, `--dir` (default `.`) | Add npm dependencies — content-addressed store, integrity-verified, JSONL lockfile, no Node. Lifecycle scripts never run. |
| `chidori install` | `--frozen` (fail instead of re-resolving — for CI), `--dir` | Install dependencies from the lockfile. |
| `chidori remove <packages…>` | `--dir` | Remove dependencies. |

See [Package Management](./package-management.md).

## Replay & testing

### `chidori resume <agent.ts> <run_id>`

Replay a recorded run byte-for-byte with zero model calls; a run that ended
at a pause or crash replays to the frontier of its journal and continues
live. The model recorded in the run manifest applies automatically. Concept
and divergence rules: [Replay & Resume](./replay.md).

| Flag | Meaning |
|---|---|
| `-d/--dir` | Default: the agent file's parent directory. |
| `--until-seq <N>` | Time travel — stop the replay at seq N. Conflicts with `--retry-failed`. |
| `--retry-failed` | Strip the trailing failed record, replay, and re-execute it live. |
| `--allow-source-change` | Resume against edited source, divergence-checked. |
| `--model` | Override the manifest-recorded model. |
| `--untrusted` / `--trusted` | Posture override. |
| `--ci` | Machine mode — see below. |

`resume --ci` exits **0** on a clean match, **3** on divergence, **1** on
error, and always prints a JSON report: `run_id`, `checkpoint_path`,
`calls_expected`, `calls_replayed`, `live_cost_usd`, recorded token counts,
`output`, `status`, and on divergence a `divergence.kind` of
`source_changed`, `missing_call`, `changed_call`, or `extra_call`. It
ignores `--model`, `--untrusted`/`--trusted`, `--until-seq`, and
`--retry-failed`.

### `chidori verify <agent.ts> <run_id>`

Replay-as-test for CI: replays with an empty provider registry, an empty
tool registry, and the `untrusted` profile, and requires the run to complete
with byte-identical output — there is no `--allow-source-change` escape.
Journaled top-level workspace writes do re-materialize to real disk (same
bytes, fresh mtime). Exits 0 on pass and **1 on any failure** — no separate
divergence code, unlike `resume --ci` — with a distinct message per failure
mode (source drift, unclean replay, a pause instead of completion, output
mismatch, unexpected live calls). Contract details:
[Replay & Resume](./replay.md).

| Flag | Meaning |
|---|---|
| `-d/--dir` | Default: the agent file's parent directory. |
| `--runs-dir <dir>` | Read the run from `<runs-dir>/<run_id>` — the consumption side of `chidori export --fixture`. |

### `chidori export <run_id> --fixture <dest>`

Copy just the four artifacts `verify` reads — `records.jsonl`,
`runtime.snapshot.json`, `output.json`, `input.json` — into
`<dest>/<run_id>/`: the fixture you commit for CI. `--fixture` is required;
`-d/--dir` defaults to the current directory.

### `chidori checkpoint export|import`

Whole-run archives. `checkpoint export <run_id>` writes
`<run_id>.chidori-run.tar.gz` (`-o/--output` overrides the name,
`-d/--dir`); `checkpoint import <archive>` unpacks under
`<dir>/.chidori/runs/`.

### Inspection

| Command | Flags | What it does |
|---|---|---|
| `chidori trace <run_id>` | `-d/--dir` | Print the run's journal — every prompt, tool call, and effect, with token counts and cost (including prompt-cache read/write totals). |
| `chidori snapshot <run_id>` | `-d/--dir` | Print `runtime.snapshot.json` metadata (never raw VM snapshot bytes). |
| `chidori history <run_id>` | `-d/--dir`, `--show <commit>` (unique hex prefix, ≥ 4 chars), `--diff <c1[..c2]>` (conflicts with `--show`), `--path <file>`, `--json` | The run's source history: the git-like chain of source versions, each anchored to the journal records that executed under it ([Source History](./source-history.md)). |
| `chidori stats` | `-d/--dir` | Usage and cost totals, including prompt-cache tokens (reads each run's `checkpoint.json`). |

**`--dir` defaults differ**: `resume` and `verify` default to the agent
file's parent directory; the inspection and recovery commands (`trace`,
`snapshot`, `history`, `stats`, `export`, `checkpoint`, `branches`,
`holdings`, `rollback`) default to the current directory.

## Branching & recovery

| Command | Flags | What it does |
|---|---|---|
| `chidori branches <run_id>` | `-d/--dir` | List a run's persisted branch stores. |
| `chidori branch-resume <run_id> <branch_id> -v "…"` | `-v/--value <response>` (required — `-v` means *value* here, not verbose), `-d/--dir`, `--model`, `--untrusted`/`--trusted` | Answer a paused `input()` inside a branch. |
| `chidori branch-rerun <run_id> <branch_id>` | `-d/--dir`, `--model`, `--untrusted`/`--trusted` | Re-run a branch's (possibly edited) `source.ts` from its fork-time anchor. |
| `chidori holdings <run_id>` | `-d/--dir` | The run's live obligations: the pending host call it is parked on, queued signals, unsettled actors, detached agents it launched (with registry state), open branches, armed compensations. Also served as `GET /sessions/{id}/holdings`. |
| `chidori rollback <run_id>` | `-d/--dir`, `--untrusted`/`--trusted` | Saga rollback: run the compensations registered with `chidori.compensation.register(...)` newest-first, each as its own ordinary run. Refuses a completed run (compensations are void on success) and a second rollback (inverse actions are not re-fired). |

Branches: [Branching Execution](./branching-execution.md). Compensations:
[Host API](./host-api.md).

## Storage & deploy

### `chidori cell-store`

Run the shared run-store server. Point `CHIDORI_RUN_STORE=http://host:9700`
at it and nothing else changes; `CHIDORI_RUN_STORE_TOKEN` adds bearer auth.
See [Durable Storage](./durable-storage.md).

| Flag | Default |
|---|---|
| `--listen` | `127.0.0.1:9700` |
| `--bucket s3://bucket[/prefix]` | Omit for single-node, no replication. |
| `--data-dir` | `.chidori/cellstore` |
| `--node-id` | Generated and persisted. |
| `--advertise URL` | Omit. The address this node is reachable at; it rides the ownership records, so a client refused with 409 is handed somewhere to go and follows it once. |
| `--lease-secs` / `--sync-secs` / `--idle-secs` | 30 / 2 / 300 |

### `chidori deploy`

Deploy an agent directory to a Chidori Deploy server (URL via `--url` /
`CHIDORI_DEPLOY_URL`) — a self-hosted, experimental service, as the default
`http://localhost:8090` URL suggests. The model is Val-Town-style: a local
directory kept in sync with the server, where each push becomes an
**immutable version** and exactly one version is live. With no subcommand,
`chidori deploy` pushes the current directory as a new live version.

Configuration resolves per field as CLI flag → environment →
`~/.chidori/credentials.json`:

- **URL**: `--url` → `CHIDORI_DEPLOY_URL` → stored `deploy_url` →
  `http://localhost:8090`.
- **Token**: `--token` → `CHIDORI_API_KEY` → stored `deploy_api_key`
  (missing is a hard error).

`--url` / `--token` work on any deploy subcommand.

| Subcommand | What it does |
|---|---|
| `login` | Browser OAuth against the deploy console (`--console <url>`, default `http://localhost:3020` or `CHIDORI_CONSOLE_URL`; `--name <label>`, default hostname); saves `deploy_url` + `deploy_api_key` to `~/.chidori/credentials.json` (owner-only permissions). |
| `push` | Push a directory as a new live version: `--dir .`, `--name <basename>`, `--entrypoint agent.ts`, `--note ""`. UTF-8 text files only (others skipped with a warning); always ignores `.git`, `.chidori`, `node_modules`, `target`, `.DS_Store`, `.env`; a `.chidoriignore` adds line-based exact-path / basename / dir-prefix rules (not full gitignore globs). Identical trees dedupe ("Up to date"). |
| `status --name <n>` | Live version, hash, entrypoint, created-at, version count. |
| `versions --name <n>` | Every version; `*` marks the live one. |
| `rollback --name <n> [--to <N>]` | Make an earlier version live (omit `--to` for the previous one). |
| `promote --name <n> <version>` | Make a specific version live. |
| `pull --name <n> [--version N] [--out <dir>]` | Bring a version's tree back down. |
| `logs --name <n> [--tail 20]` | Recent runs. |
| `watch [--dir .] [--name] [--entrypoint agent.ts] [--interval-ms 800]` | Push on change until Ctrl-C. |
| `list` (alias `ls`) `[--watch] [--interval 5]` | All deployments. |
| `schedule create <name> --cron "<expr>" [--input <json>] [--disabled]`, then `schedule list` / `delete <id>` / `pause <id>` / `resume <id>` / `add <id> <agent>` / `remove <id> <agent>` | Cron-fired runs (5- or 6-field expressions). |
| `fleet [--window <hours>]` | Cross-agent activity overview (default window 168 hours). |

## Approval postures

The posture decides what happens when an agent reaches a **gated effect**.
Exactly four target families are gated: network (`fetch` / `node:http`),
`chidori.tool` calls, workspace access (writes and deletes ask; list, read,
and manifest route through the same gate but are allowlisted in both
built-in profiles), and app-data. LLM prompts and pure compute are never
gated.

| Context | Behavior |
|---|---|
| Bare `chidori run` (also `dev`, `chat`, `resume`, and the branch commands) | The `supervised` profile: each gated effect asks at the terminal — `[y]es once / [a]ll further calls to this target / [N]o`. A `y` answer is remembered for identical arguments for the rest of the run; `a` approves all further calls to that target for the run. The prompt opens the terminal directly, so it works even with piped stdin. |
| `chidori run` with no terminal (scripts, CI) | Fail closed, with an error naming `--trusted` and the `CHIDORI_POLICY*` variables. |
| `CHIDORI_POLICY_AUTO_APPROVE=1` | Auto-approves ask-gated calls; it never overrides a deny rule. |
| Bare `chidori serve` | The `untrusted` profile: deny by default, with read-only workspace introspection (list/read/manifest) still allowed. |
| `--trusted` | The permissive allow-all posture. |
| `--untrusted` | Forces deny-by-default, and wins over any environment configuration. |

Explicit policy configuration takes precedence in the order
`CHIDORI_POLICY_FILE` → `CHIDORI_POLICY` → `CHIDORI_POLICY_PROFILE` → the
command's default. The full model — profiles, policy files, per-session
overlays — is in the [Sandbox Model](./sandbox-model.md).

## Exit codes

Every command exits 0 on success and 1 on failure, with two exceptions:

- `chidori check` exits **2** on validation failure.
- `chidori resume --ci` exits **0** on a clean match, **3** on divergence,
  and **1** on error, always with a JSON report.

`chidori verify` is plain 0/1 — no separate divergence code — but each
failure mode gets a distinct message.
