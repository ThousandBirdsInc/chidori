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
durability tier, running on easy-to-provision hosts (Fly.io, Railway, Render)
or an existing Kubernetes cluster, what high availability looks like today,
and the current scaling limits.

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

## Easy-to-provision hosts: Fly.io, Railway, Render

If you'd rather not manage a VM, any platform that runs a container as a
**long-lived process** hosts Chidori well. The recipe is the same everywhere:
the Dockerfile above, an `s3://` run-store mirror so the machine is
disposable, TLS and a public hostname from the platform, and secrets via the
platform's secret store. Two platform behaviors to check before choosing:

- **No scale-to-zero.** Detached-agent alarms fire from a timer in the
  running server ([detached agents](./detached-agents.md)); paused runs and
  signal deliveries also need a process to arrive at. A platform that stops
  idle machines silently stalls them until the next request. Pin at least one
  instance always-on.
- **One instance per agent.** Runs have a single writer
  ([see below](#higher-availability)); set the platform's instance count to 1
  rather than letting it autoscale replicas behind one hostname.

### Fly.io walkthrough

Fly runs your container as a Machine (a real VM), terminates TLS for you, and
replaces failed machines automatically — a good match for a stateful
single-process server.

```toml
# fly.toml
app = "my-agent"
primary_region = "iad"

[env]
  CHIDORI_DURABILITY = "strict"
  CHIDORI_RUN_STORE = "s3://my-agent-runs"      # any S3-compatible bucket;
                                                # Fly's Tigris storage speaks
                                                # the S3 API too
[http_service]
  internal_port = 8080
  force_https = true
  auto_stop_machines = "off"      # keep alarms/signals live (see above)
  min_machines_running = 1

  [[http_service.checks]]
    path = "/health"
    interval = "15s"
    timeout = "2s"
```

```bash
fly launch --no-deploy                # detects the Dockerfile
fly secrets set ANTHROPIC_API_KEY=... CHIDORI_API_KEY=... \
  AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
fly deploy
fly scale count 1                     # one writer; don't add replicas
```

With the `s3://` mirror there is no volume to manage: if Fly replaces the
machine (hardware failure, `fly deploy`), the fresh one hydrates any run it's
asked about from the bucket, and the detached-agent fleet re-arms from the
mirrored registry at boot. A Fly volume mounted at `/app/.chidori` also works,
but pins the machine to one host — the mirror is the more available choice.

**Railway** and **Render** are the same shape with different spelling: both
build the Dockerfile from your repo, give you a TLS hostname, restart crashed
services, and hold secrets. On Railway keep the service on a plan/setting
without app sleeping; on Render use a Web Service (not a free-tier instance,
which spins down when idle) and set the health check path to `/health`. On
both, either attach their persistent disk at `/app/.chidori` or — better —
skip the disk and use the `s3://` mirror.

### Where Vercel (and serverless functions) fit

Vercel, Netlify, and Lambda-style platforms run request-scoped functions, not
long-lived processes with disks — they can't host the `chidori` binary itself
(nowhere for the server to keep listening, hibernating agents to wake, or
alarms to fire). Use them for what they're good at, next to a Chidori host:

- **Frontend/API on Vercel, runtime elsewhere.** Your Vercel app drives a
  Chidori server on Fly/Railway/a VM over HTTP with the
  [TypeScript SDK](../sdk/typescript/README.md) (or plain `fetch` against the
  [session API](./running-modes.md#2-http-server-event-driven--session-api)).
  Set `CHIDORI_CORS_ORIGINS` if the browser calls Chidori directly.
- **The one serverless piece that does exist** is on the storage side: the
  [Cloudflare Durable Object run store](../integrations/cloudflare-durable-objects/)
  is a Worker you deploy with `wrangler deploy`, and it only ever runs when a
  journal write or hydration read arrives.

## Deploying to an existing Kubernetes cluster

If your team already runs Kubernetes, Chidori is an easy tenant: one
container, one port, one health endpoint, state either on a PVC or (better)
in an `s3://` mirror. The constraints from the managed-hosts section translate
directly into manifest choices:

- **`replicas: 1` and `strategy: Recreate`.** One writer per run means no
  replicas behind the Service — and the default `RollingUpdate` strategy
  briefly runs old and new pods side by side, which is exactly the
  double-driver overlap that `fs`/`s3://` leases only police advisorily
  ([higher availability](#higher-availability)). `Recreate` stops the old pod
  before starting the new one. (With the SQLite backend on a PVC or the
  Durable Object relay, overlap is enforced away, but there's still no reason
  to roll.)
- **No HPA on this workload.** Scale up via resources +
  `CHIDORI_MAX_CONCURRENT_SESSIONS`; scale out by adding a Deployment per
  agent and routing by host/path at the Ingress.
- **Kubernetes restarts are the recovery path.** Liveness-probe restarts and
  node evictions are the same event as a crash: the pod comes back, hydrates
  from the mirror, re-arms the detached-agent fleet, and resumes runs by
  replay.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-agent
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels: { app: my-agent }
  template:
    metadata:
      labels: { app: my-agent }
    spec:
      containers:
        - name: chidori
          image: registry.example.com/my-agent:v1   # the Dockerfile above
          ports:
            - containerPort: 8080
          env:
            - name: CHIDORI_DURABILITY
              value: "strict"
            - name: CHIDORI_RUN_STORE
              value: "s3://my-agent-runs"
          envFrom:
            - secretRef:
                name: my-agent-secrets   # ANTHROPIC_API_KEY, CHIDORI_API_KEY,
                                         # AWS_ACCESS_KEY_ID/SECRET_ACCESS_KEY
          readinessProbe:
            httpGet: { path: /health, port: 8080 }
            periodSeconds: 10
          livenessProbe:
            httpGet: { path: /health, port: 8080 }
            periodSeconds: 15
            failureThreshold: 3
          resources:
            requests: { cpu: "500m", memory: "1Gi" }
            limits: { memory: "4Gi" }   # pair with CHIDORI_JS_MEM_CAP_MB
---
apiVersion: v1
kind: Service
metadata:
  name: my-agent
spec:
  selector: { app: my-agent }
  ports:
    - port: 80
      targetPort: 8080
```

Put your standard Ingress (or Gateway) with TLS in front of the Service, and
keep `CHIDORI_API_KEY` set even inside the cluster — the session API is
otherwise open to anything that can reach the Service.

**State.** The manifest above is deliberately stateless: with the `s3://`
mirror, a rescheduled pod hydrates whatever it's asked about, so it can land
on any node. The alternative is a `ReadWriteOnce` PVC mounted at
`/app/.chidori` (use a StatefulSet in that case); it works, but couples the
pod to a volume the same way a Fly volume couples a machine to a host — reach
for it only when no object store is available, and note that the in-cluster
S3 answer (a MinIO deployment) keeps even that dependency inside the cluster.

**If you use `--isolate`.** The OS-isolation layers degrade predictably in a
container: the seccomp denylist is the required core
(`CHIDORI_ISOLATE_REQUIRE_SANDBOX=1` makes its absence fatal rather than a
warning), Landlock needs a 5.13+ node kernel, and the empty network namespace
needs `CAP_SYS_ADMIN` — which most clusters rightly deny, so expect that layer
to be skipped and rely on the policy gate plus your cluster's NetworkPolicy
for egress control. See [sandbox model](./sandbox-model.md).

## Higher availability

Be precise about the constraint first: **a run has one writer at a time.**
Leases arbitrate ownership, but nothing routes requests to a run's owner
([durable storage](./durable-storage.md)), so "two identical servers behind a
load balancer" is not the HA model today. What you can have — and what
matters for agent workloads — is **no lost work and fast, automatic
replacement**:

1. **RPO ≈ 0: strict durability + a remote mirror.** With
   `CHIDORI_DURABILITY=strict` and an `s3://` or Durable Object mirror, every
   acknowledged side effect has a durable recording. Whatever dies, no
   completed work is lost — recovery replays the journal.
2. **RTO = a restart: auto-replacement + hydration.** Fly/Railway/Render
   replace a dead machine (or systemd restarts the process) with no state to
   restore: the new instance hydrates runs from the mirror on demand and
   re-arms the detached-agent fleet from the mirrored registry at boot.
   In-flight runs resume at their last safepoint by replay; paused runs stay
   paused and answerable.
3. **A lingering old instance stands down.** If the platform briefly runs old
   and new instances side by side during replacement, run leases keep them
   from double-driving. Note the backend difference: the SQLite backend and
   the Durable Object relay **enforce** the single writer; `fs` and `s3://`
   leases are advisory (last-writer-wins) — so on platforms that overlap
   instances during deploys, prefer the Durable Object relay, or configure
   the deploy strategy to stop the old machine before starting the new one.
4. **Warm standby (active–passive).** For faster manual failover, keep a
   second instance configured identically against the same mirror but
   stopped; promoting it is starting it. Don't run it hot against the same
   store — that's the double-driver shape leases exist to police, not a
   supported active–active mode.
5. **Horizontal capacity by sharding, not replicating.** To scale beyond one
   machine, run one server per agent (or per tenant), each with its own state
   directory and mirror prefix, and route by hostname or path at your proxy.
   Each shard gets the HA recipe above independently.

The strongest write story is the
[Durable Object relay](../integrations/cloudflare-durable-objects/): every
acknowledged write is replicated across data centers before Chidori proceeds,
you get 30-day point-in-time recovery, and the platform guarantees one live
object per run — so failover can't fork a run's history even in the overlap
window. Multi-node request routing (true active–active) is future work;
see the non-goals in [durable storage](./durable-storage.md).

## Scaling and topology limits

- **Scale up before out.** The server is a single Rust process with an
  embedded JS engine per run; raising `CHIDORI_MAX_CONCURRENT_SESSIONS` on a
  bigger machine is the intended growth path. Going wider means sharding by
  agent as described [above](#higher-availability) — not replicas behind one
  load balancer.
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
- [ ] On managed hosts / Kubernetes: one instance per agent (`replicas: 1`, `strategy: Recreate`), scale-to-zero/app-sleep disabled, no HPA
