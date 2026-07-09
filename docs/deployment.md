# Deploying Chidori

Chidori is one self-contained binary, so a deployment is small: put the binary
and your agent's `.ts` files on a machine, set a handful of environment
variables, and run `chidori serve`. There is no Node runtime to provision, no
database server to stand up, no queue, and no worker fleet — the durability
story is the [effect journal](./replay.md) written to plain files, optionally
mirrored somewhere that survives the machine
([durable storage](./durable-storage.md)).

This document covers taking that from a laptop to production: the recommended
zero-dependency single-machine setup, hardening the HTTP server, choosing a
durability tier, and the current scaling limits.

## What a deployment consists of

- **The `chidori` binary** — the runtime. Install it with the
  [release install script](../README.md#0-install) or build it with
  `cargo build --release`; either way it is a single static-ish executable you
  can copy between machines.
- **Your agent project** — plain `.ts` files (plus `tools/`, `node_modules`
  from [`chidori install`](./package-management.md), and any workspace files).
  The server serves one agent file: `chidori serve agent.ts`.
- **A state directory** — everything a run persists lives in `.chidori/`
  next to the agent file: run journals under `.chidori/runs/<run_id>/`, the
  detached-agent registry under `.chidori/runs/agents/`, and (if configured)
  the SQLite mirrors. Back this directory up and you have backed up every run.
- **Environment variables** — a model provider key (`ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `LITELLM_API_URL`, or the OpenRouter credentials from
  `chidori model-login`), plus the server/durability settings below.

## The zero-dependency deployment: one binary, one machine, one disk

This is the easiest path and the one to start with. It has **no dependency on
any specialized provider**: no managed cloud services, no object store, no
Cloudflare account, no container platform. Any Linux VM from any host — or a
machine under your desk — works, and moving off it later is copying one
directory. Durability comes from the journal on the local disk plus a
single-file SQLite mirror on the same disk, which is enough to survive
crashes, restarts, and redeploys (see [tiers](#durability-tiers-what-survives-what)
below for when it isn't enough).

1. **Install the binary and the project:**

   ```bash
   curl -fsSL https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/scripts/install.sh | sh
   sudo mv ~/.chidori/bin/chidori /usr/local/bin/
   sudo mkdir -p /opt/my-agent && sudo chown chidori: /opt/my-agent
   # copy agent.ts (and tools/, node_modules/) into /opt/my-agent
   ```

2. **Write the environment file** (`/etc/chidori/env`, mode `0600`):

   ```bash
   ANTHROPIC_API_KEY=sk-ant-...
   # Require a bearer token on every non-/health request:
   CHIDORI_API_KEY=<long random string>
   # Persist the session index across restarts (it is in-memory otherwise):
   CHIDORI_DB_PATH=/opt/my-agent/.chidori/sessions.sqlite3
   # Mirror every run's journal into one SQLite file beside the run dirs:
   CHIDORI_RUN_STORE=sqlite
   # Refuse to act on the world without a durable recording of it:
   CHIDORI_DURABILITY=strict
   ```

3. **Run it under systemd** (`/etc/systemd/system/chidori.service`):

   ```ini
   [Unit]
   Description=Chidori agent server
   After=network-online.target
   Wants=network-online.target

   [Service]
   User=chidori
   WorkingDirectory=/opt/my-agent
   EnvironmentFile=/etc/chidori/env
   ExecStart=/usr/local/bin/chidori serve agent.ts --port 8080
   Restart=always
   RestartSec=2

   [Install]
   WantedBy=multi-user.target
   ```

   ```bash
   sudo systemctl enable --now chidori
   curl http://localhost:8080/health
   ```

4. **Back up one directory.** All state is `/opt/my-agent/.chidori/` — journals,
   checkpoints, the SQLite mirrors, the detached-agent registry. `rsync` it (or
   snapshot the disk) on whatever cadence your recovery point requires. Restoring
   onto a fresh machine is: install the binary, copy the project directory back,
   start the service. Paused runs resume where they stopped.

Why this setup recovers well: the journal is checkpointed at every host
safepoint, so `Restart=always` plus resume-by-replay means a crashed or
redeployed server picks its runs back up instead of losing them. At boot,
`chidori serve` also re-arms the whole [detached-agent](./detached-agents.md)
fleet from the durable registry — hibernating agents get their alarm deadlines
back, and agents that were mid-run when the previous process died are woken and
continue at the frontier. `CHIDORI_RUN_STORE=sqlite` additionally serializes
writers through one connection, giving you an enforced single-writer per run
without any external service.

What this setup does *not* survive is losing the disk itself. If the machine's
storage is ephemeral, or "restore from last night's rsync" is not an acceptable
recovery point, add a remote mirror — the next tier up still doesn't require a
specialized provider.

## Hardening the HTTP server

`chidori serve` binds `0.0.0.0:<port>` and exposes the
[session API](./running-modes.md#2-http-server-event-driven--session-api).
Before pointing the internet at it:

- **Auth.** Set `CHIDORI_API_KEY`; every request except `GET /health` then
  requires `Authorization: Bearer $CHIDORI_API_KEY`. Without it the API is
  open to anyone who can reach the port.
- **TLS.** The server speaks plain HTTP. Terminate TLS in front of it with a
  reverse proxy (Caddy makes this two lines: `example.com { reverse_proxy
  localhost:8080 }`) or your load balancer, and firewall the port so only the
  proxy reaches it.
- **Policy.** The server is **deny-by-default**: unless you configure a policy
  (`CHIDORI_POLICY`, `CHIDORI_POLICY_FILE`, or
  `CHIDORI_POLICY_PROFILE=untrusted|supervised`) or pass `--trusted`, gated
  effects — network requests via `fetch`/`node:http`, workspace mutations — are
  refused, and a
  *malformed* policy fails closed rather than falling back to allow-all. For a
  server running only your own code, `--trusted` restores the permissive local
  default; better is an explicit allowlist via `CHIDORI_POLICY_FILE`. See
  [sandbox & security model](./sandbox-model.md).
- **Isolation.** If sessions run code you don't fully trust, add
  `--isolate` / `CHIDORI_ISOLATE=process`: each run executes in a disposable,
  OS-sandboxed child process (Linux: empty network namespace + Landlock +
  seccomp) that brokers every effect back through the policy gate.
- **CORS.** Off by default. Browser frontends need
  `CHIDORI_CORS_ORIGINS=https://app.example.com` (comma-separated; `*` opens it).
- **Concurrency.** `CHIDORI_MAX_CONCURRENT_SESSIONS` (default 8) caps parallel
  runs so one burst can't flood your LLM provider;
  `CHIDORI_ACQUIRE_TIMEOUT_MS` (default 30000) bounds how long a request waits
  for a slot.
- **Secrets.** Beyond provider keys, secrets can ride `CHIDORI_SECRET_ENV` — a
  host-only JSON map of placeholder token → value with a per-secret host
  allowlist. Agent code and the journal only ever see the
  `__CHIDORI_SECRET__…__` placeholder; the runtime substitutes the real value
  into outbound requests just before the wire, and only for allowlisted hosts
  (`crates/chidori/src/runtime/secret_env.rs`). Checkpoints stay safe to
  commit and mirror.

## Durability tiers: what survives what

The [run store](./durable-storage.md) is layered — local disk is always the
fast primary, and `CHIDORI_RUN_STORE` picks an optional mirror. Each tier is a
strict upgrade in what it survives; none of them changes agent code.

| Tier | Config | Survives | Depends on |
|---|---|---|---|
| Local disk (default) | unset / `fs` | process crash, restart, redeploy | nothing |
| SQLite mirror | `sqlite` (+ `CHIDORI_RUN_DB`) | the above, + single-file backup/restore; serialized writers | nothing |
| S3-compatible object store | `s3://bucket/prefix` | **machine loss** — runs hydrate back on a fresh box | any S3 API: AWS S3, R2, GCS, Backblaze, or self-hosted MinIO (still provider-neutral) |
| Durable Object relay | `https://…workers.dev` | machine loss, + cross-datacenter replication, 30-day PITR, platform-enforced single writer per run | Cloudflare ([integration](../integrations/cloudflare-durable-objects/)) |

Two settings apply at every tier:

- `CHIDORI_DURABILITY=strict` gates side effects on journal durability: a
  failed journal write poisons the run rather than letting it keep acting on
  the world unrecorded, and results aren't surfaced until their journal is
  flushed. Use it in production; the `besteffort` default is for local dev.
- After machine loss with a remote mirror, recovery is automatic **hydration**:
  on a fresh machine with the same `CHIDORI_RUN_STORE` env, `chidori resume`
  or a server session load materializes the run directory from the mirror and
  proceeds as if the files had always been there. Do a recovery drill once —
  it's a genuinely boring procedure, which is the point.

## Containers (optional, not required)

There is nothing container-shaped about the runtime — no dynamic linking
surprises, no sidecar. If your infrastructure is container-first, a minimal
image is:

```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSL https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/scripts/install.sh | sh \
    && mv /root/.chidori/bin/chidori /usr/local/bin/ \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
EXPOSE 8080
CMD ["chidori", "serve", "agent.ts", "--port", "8080"]
```

Mount a volume at `/app/.chidori` (or skip the volume and configure an
`s3://` mirror — hydration makes the container disposable). Everything in the
systemd section applies unchanged; the env file becomes container env.

## Scaling and topology limits

Be deliberate about what the current storage layer does **not** do
(documented in [durable storage](./durable-storage.md)):

- **One process drives a run at a time.** Leases (`lease.json`, TTL-based)
  arbitrate ownership so two servers sharing a mirror won't double-execute a
  run — but nothing *routes* a request to the run's owner. Don't put a
  round-robin load balancer in front of two servers sharing a store and expect
  session affinity; today the supported shapes are one server, or several
  servers each owning disjoint agents.
- **Scale up before out.** The server is a single Rust process with an
  embedded JS engine per run; raising `CHIDORI_MAX_CONCURRENT_SESSIONS` on a
  bigger machine is the intended growth path. For fleets, shard by agent:
  one `chidori serve` per agent file, each with its own state directory.
- **Replay cost is O(run history).** Resuming a very long run replays its
  journal (fast, zero LLM calls — see
  [resume performance](./resume-performance.md) for numbers and direction).

## Upgrades

Replace the binary and restart the service. On boot the server reloads
sessions from `CHIDORI_DB_PATH`, re-arms the detached-agent fleet from the
registry, and paused runs resume by replay. The journal format ships with the
binary, so treat a running fleet's runs as tied to close binary versions:
prefer draining or settling long-lived runs before large version jumps, and
keep the previous binary around until the new one has driven your runs.

## Production checklist

- [ ] `CHIDORI_API_KEY` set; port firewalled behind a TLS proxy
- [ ] Policy configured explicitly (`CHIDORI_POLICY_FILE` or a deliberate `--trusted`)
- [ ] `CHIDORI_DB_PATH` set so sessions survive restarts
- [ ] `CHIDORI_RUN_STORE` chosen (at minimum `sqlite`; `s3://` if the disk is ephemeral)
- [ ] `CHIDORI_DURABILITY=strict`
- [ ] `.chidori/` backed up, or a mirror + hydration drill done
- [ ] `Restart=always` (or the container equivalent) so resume-by-replay can do its job
