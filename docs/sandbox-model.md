# Sandbox model of the chidori-js runtime

> Two layers ship: the default **in-process** capability-confinement
> sandbox, and an **opt-in OS-level isolation** mode (`--isolate`) that runs each
> agent in a confined child process — see
> [OS-level isolation](#os-level-isolation-opt-in---isolate). Known limitations
> are documented in [Current gaps](#current-gaps).
> **Engine:** the pure-Rust `chidori-js` engine — the only JS engine in
> the tree.
> **Related:** [`docs/os-isolation-plan.md`](./os-isolation-plan.md),
> [`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md),
> [`docs/conformance.md`](./conformance.md).

This document describes what kind of sandbox the `chidori-js` runtime is, what it
confines, what it deliberately does *not* confine, and which protections are
on by default. It is written for two readers: someone deciding whether it is safe
to run a given class of code on this engine, and someone hardening it further.

## TL;DR

`chidori-js` is a **capability-confinement sandbox built on Rust memory safety**,
not an OS-level containment sandbox. The JavaScript it runs has **no ambient
authority**: the interpreter itself cannot touch the filesystem, network, clock,
or processes. Every capability is something the host explicitly *injects*. On top
of that, per-run **resource limits** (opcode budget, memory ceiling, optional
wall-clock deadline) bound runaway CPU and heap so a single agent cannot hang or
OOM the host.

What it is **good** at: confining the *language* — a buggy or hostile script
cannot corrupt memory, escape into host code, or reach a capability it was not
given.

What it is **not** (by default): a containment boundary for the powerful effects
the host *does* inject (`http`, `workspace.*`). Those are real capabilities;
whether granting them is "safe" depends entirely on whether the agent code is
trusted.

For that case there is an **opt-in OS-level isolation mode**
(`--isolate` / `CHIDORI_ISOLATE=process`): each run executes in a disposable
child process that holds *only* the JS engine and brokers every effect back to
the trusted parent over a pipe. The child runs under a per-OS sandbox (Linux:
empty network namespace + Landlock read-only filesystem + seccomp syscall
denylist; macOS: a Seatbelt deny profile) plus a `setrlimit` floor and a
parent-side deadline-kill — so even a total compromise of the interpreter has no
ambient network, filesystem, or sibling-run to reach. See
[OS-level isolation](#os-level-isolation-opt-in---isolate).

## Threat model

The framework's primary model is **trusted, developer-authored agent code**. The
"sandbox" exists chiefly to make execution *deterministic and replayable* (route
all non-determinism through captured effects) and to provide *defense-in-depth*
against bugs and accidental resource exhaustion — see the product framing in
[`docs/conformance.md`](./conformance.md) (language conformance measured bare,
with the security sandbox and deterministic replay layered on top).

This doc additionally evaluates the engine against a stricter model —
**untrusted agent code** — because the architecture (no ambient authority +
memory safety) is most of the way there, and it is useful to be precise about the
remaining distance.

| Adversary capability | Confined? |
|---|---|
| Corrupt host memory / execute native code via an interpreter bug | ✅ Yes — pure safe Rust |
| Reach fs / net / clock / processes the host did not inject | ✅ Yes — no ambient authority |
| Escape the virtual filesystem to real disk via `node:fs` | ✅ Yes — in-memory VFS, traversal-clamped |
| Hang the host with an infinite loop | ✅ Yes — opcode budget |
| OOM the host (string / heap growth) | ✅ Yes — string cap + memory ceiling |
| Crash the host with a panic | ✅ Yes — `catch_unwind` boundary |
| Abuse an injected powerful effect (`http`, `workspace`) | ✅ On the server (deny-by-default unless the operator opts out); ⚠️ on the bare CLI only if the operator gates it — see [gaps](#current-gaps) |
| Starve co-tenant agents / exceed a per-agent memory quota | ✅ Per-run meter (thread-attributed; small cross-thread drift) — see [gaps](#current-gaps) |
| Break out of the process / OS | ⚠️ Default: none (in-process). ✅ With `--isolate`: confined child process — seccomp/Seatbelt + netns/Landlock + rlimits — see [OS-level isolation](#os-level-isolation-opt-in---isolate) |

## Architecture: capability injection, not ambient authority

The pure engine (`crates/chidori-js`) is a parser (`oxc`) → bytecode compiler →
stack VM with `Rc<RefCell>` reference counting. Two properties make it a sandbox:

1. **Memory safety.** There is **zero `unsafe`** in the engine crate (the word
   appears only in doc comments). The
   whole stack is safe Rust plus `oxc`. The worst an interpreter bug can do is
   panic or misbehave — it cannot corrupt memory or jump into host code. This is a
   categorical improvement over embedding a C/C++ engine (QuickJS, V8) in-process.

2. **No ambient authority.** The engine contains no `std::fs`, `std::net`,
   `std::process`, `std::thread`, `Command`, or HTTP client. A bare
   `Engine::new()` running arbitrary JS can compute and allocate — nothing else.
   I/O exists **only** because the host installs it:
   - `Engine::install_chidori_effects` (`crates/chidori-js/src/lib.rs`) wires the
     async `chidori.*` effect surface (`log`, `tool`, `prompt`, `input`,
     `memory`, `template`, `checkpoint`, `callAgent`, `workspace.*`). Networking
     is **not** a `chidori.*` method — it is brokered through the internal
     `globalThis.__chidori_http` global (forwarded as the `"http"` effect),
     reached only via the captured `fetch`/`node:http` surface. The
     `execJs`/`execPython`/`execWasm` JS stubs are defined but inert — the
     host backend rejects the effect (`… is not supported on the rust engine`);
     there are no snippet sandboxes.
   - `Engine::install_sync_natives` wires the synchronous `__chidori_*` natives the
     `node:` shims call (crypto hashing/HMAC, captured randomness, the VFS).

   JS cannot name a capability it was not handed. Determinism follows from the same
   seam: every non-deterministic source flows through a host effect that is
   recorded in record mode and replayed in replay mode (`crates/chidori-js/src/host.rs`).

The production wiring lives in `src/runtime/rust_engine.rs::run_module`, which
builds a fresh engine per run, installs the captured-effect natives + determinism
prelude, forwards `chidori.*` to the shared `HostBindingBackend` (the durable
host machinery — call log, replay, policy, MCP, OTEL), then runs the agent's
entrypoint.

## Filesystem isolation (`node:fs` → VFS)

`node:fs` does **not** reach real disk. It maps to an in-memory, snapshot-resident
virtual filesystem on `RuntimeContext` (`src/runtime/context.rs` `vfs_*` →
`src/runtime/vfs.rs`). Key properties:

- **In-memory / snapshot-resident.** Writes ride the snapshot manifest and survive
  suspend→restore; a write never touches the host disk.
- **Path-traversal clamped.** `normalize()` resolves `.`/`..` and clamps `..` at
  the root, so `/../../etc/passwd` resolves *inside* the VFS, not to a real file.
- **Host-disk mode is unimplemented.** `FsPolicy::Host` is explicitly rejected
  (`src/runtime/rust_engine.rs` `fs_policy_guard`); the durable default is
  `FsPolicy::Captured`. `crypto` and `timers` are likewise policy-gated
  (`CryptoPolicy`, `TimerPolicy`).

`workspace.*` is different: it performs **real disk I/O**, but is sanitized to a
workspace root with `..`/absolute-path rejection and symlink-traversal checks
(host side, `src/runtime/workspace.rs`). It is a deliberate capability, not an
isolated surface.

## Resource limits (DoS protection)

The engine exposes the primitives; `run_module` wires them per run via an RAII
`ExecutionGuard` (`src/runtime/rust_engine.rs`). All are env-tunable so a
deployment can tighten them for untrusted code or relax them for trusted batch work.

### Opcode budget — bounds CPU

`vm.op_budget` decrements once per executed opcode and throws an uncatchable
`RangeError` at zero (`crates/chidori-js/src/exec.rs`). It bounds **pure-JS
compute** and is **latency-independent**: time spent blocked in a synchronous host
effect (an LLM/tool/http call) does *not* consume it, so a legitimately slow agent
is unaffected while `while (true) {}` still terminates.

- Env: `CHIDORI_JS_OP_BUDGET` (default `5_000_000_000`; `0` disables).

### Memory ceiling — bounds heap

Two complementary layers:

1. **Per-op string cap (always on, in-engine).** `op_add` and `ConcatStrings`
   (template join) throw `RangeError` when a single concatenation would exceed
   `MAX_STRING_LEN` (2^28 = ~268M code units, `crates/chidori-js/src/value.rs`). This
   closes the exponential `s += s` / `` s = `${s}${s}` `` OOM
   and matches the caps on `repeat`/`padStart`/`padEnd` and on
   dense-array allocation (`MAX_DENSE_ARRAY` = 2^25 = ~33.5M elements). With these
   caps, no *single* opcode can allocate without bound.

2. **Per-run live-heap ceiling (watchdog).** A `CountingAllocator`
   (`src/mem_guard.rs`) is installed as the binary's `#[global_allocator]`. Each
   run registers a **per-run meter** on its execution thread (`RunMeterGuard`);
   every alloc/free performed on that thread is charged to that run, so under
   concurrent multi-agent execution one run's allocations are attributed to that
   run rather than to whichever co-tenant happens to be sampled (nested
   `callAgent` children meter themselves and hand accounting back to the
   parent). A background watchdog samples the meter and trips the VM's
   cooperative-cancellation flag (`vm.interrupt`, polled every 256 ops) when a
   run exceeds its cap — catching the vector a per-op cap cannot: accumulating many
   capped objects in a long-lived container (`Map`/`Set`/array). The VM unwinds with
   `RangeError: execution interrupted`.

   - Env: `CHIDORI_JS_MEM_CAP_MB` (default `4096`; `0` disables) and
     `CHIDORI_JS_MEM_POLL_MS` (watchdog sampling interval, default `10`).

### Wall-clock deadline — optional, off by default

The same watchdog can enforce a wall-clock deadline, also via `vm.interrupt`.

- Env: `CHIDORI_JS_DEADLINE_MS` (default `0` = off).
- **Caution:** wall-clock time includes time blocked in *synchronous host effects*
  (LLM/tool/http run inline on the run thread), so a tight deadline can abort an
  agent merely *waiting* on a slow tool. It is off by default for that reason —
  prefer the opcode budget to bound compute. Enable the deadline only where host
  effects are known-fast (e.g. confining untrusted code under a short hard limit).

### Stack & regex — always on

- `max_call_depth = 2000` guards native-stack overflow from deep JS recursion
  (`crates/chidori-js/src/vm.rs`).
- `REGEX_STEP_LIMIT = 100_000` bounds catastrophic backtracking / ReDoS
  (`crates/chidori-js/src/regexp.rs`).

### Panic containment

`run_module` wraps the engine call in `std::panic::catch_unwind`
(`AssertUnwindSafe`), so an interpreter panic surfaces as
`Error: rust engine panicked: …` instead of unwinding into the host/server.

### Defaults summary

| Control | Env var | Default | Disable |
|---|---|---|---|
| Opcode budget | `CHIDORI_JS_OP_BUDGET` | `5_000_000_000` | `0` |
| Memory ceiling (MB, per-run meter) | `CHIDORI_JS_MEM_CAP_MB` | `4096` | `0` |
| Memory watchdog poll interval (ms) | `CHIDORI_JS_MEM_POLL_MS` | `10` | — |
| Wall-clock deadline (ms) | `CHIDORI_JS_DEADLINE_MS` | off | — |
| String length | (compile constant) | 2^28 (~268M) code units | — |
| Dense array length | (compile constant) | 1,000,000 | — |
| Call depth | (compile constant) | 2,000 | — |
| Regex steps | (compile constant) | 100,000 | — |

## OS-level isolation (opt-in: `--isolate`)

Everything above confines the *language* and bounds resources, but by default the
VM runs **in-process** with the host: there is no OS boundary, so a hypothetical
interpreter RCE would land in the host process. The `--isolate` mode adds that
boundary. It is **off by default** (in-process stays the default for trusted
local dev) and **additive** — agent code, the SDKs, the durable call log, and
replay semantics are byte-for-byte unchanged (asserted by
`rust_engine::tests::isolated_run_matches_in_process_byte_for_byte`). The full
design lives in [`docs/os-isolation-plan.md`](./os-isolation-plan.md); this
is the operator-facing summary. Code: `crates/chidori/src/runtime/isolate/`.

### Process-per-run with brokered effects

Each run executes in a **disposable child process** (`chidori __run-worker`, a
hidden subcommand) that holds *only* the JavaScript engine. Every host op — each
`chidori.*` effect (`log`, `prompt`, `tool`, `callAgent`, `http`, `memory`,
`template`, `checkpoint`, `input`, `workspace.*`), every captured sync native
(VFS read/write, crypto hash/HMAC/random, DOM render), and every module load — is
**RPC'd back to the parent** over a length-prefixed JSON frame protocol on the
child's stdin/stdout (`isolate/protocol.rs`). The parent keeps doing all real I/O
and owns the durable call log, policy gate, MCP, providers, and OTEL; the child
only computes JavaScript.

This is cheap because the host-call boundary was *already* a single synchronous
`(op, args) -> Result<Value, String>` seam (`route_host_op`). Brokering swaps the
in-process `InProcessHost` for a `BrokeredHost` whose dispatch is a blocking pipe
round-trip — semantically identical to the VM, which already blocks on effects
inline. The cost is one IPC hop per effect, dwarfed by LLM/tool latency.

- **Disposable, leak-free.** The child exits after one run, so the `Rc<RefCell>`
  cross-run cycle-leak concern (gap #6) does not apply to the isolated path, and
  the per-run heap meter becomes a clean per-process measure (no cross-tenant
  attribution drift — gaps #2/#3).
- **Pause/resume/replay for free.** Pause = the child returns the pause sentinel
  and exits; resume/replay = the parent spawns a fresh worker and serves recorded
  effect results over the same pipe (the child cannot tell record from replay).
  **Zero** child-state serialization.
- **Parent hardening.** A `MAX_FRAME_BYTES` (64 MB) ceiling stops a hostile child
  from OOMing the parent with a giant frame; EOF / decode error / child death all
  map to a run failure; reads never block past the deadline.

### Resource floor (all platforms)

Before any agent code runs, the worker applies a `setrlimit` floor to itself
(`isolate/limits.rs`), shipped from the parent in the `Init` frame so the parent
owns the policy:

- `RLIMIT_CPU` — hard CPU-seconds backstop to the in-engine opcode budget. Like
  the budget (and unlike a wall-clock deadline) it does **not** count time the
  child spends blocked on a brokered effect, so it bounds runaway *compute*
  without penalizing a legitimately slow agent. Opt-in (`CHIDORI_ISOLATE_CPU_SECS`).
- `RLIMIT_FSIZE` — max bytes writable to a regular file (the child has no
  filesystem, so this is belt-and-suspenders). Opt-in only, because a `0` cap
  also kills writes to a redirected regular-file `stderr` — file-write
  confinement is Landlock's job instead.
- `RLIMIT_CORE = 0` — no core dumps (a crash must not splatter memory to disk).
- `RLIMIT_NOFILE` — a small open-file ceiling (default 256).

Deliberately **not** set: `RLIMIT_AS` (address-space caps are too blunt — a
multi-threaded VM over-reserves virtual memory) and `RLIMIT_NPROC` (counts every
process of the real uid, fragile under shared-uid concurrency; blocking `fork`
belongs to seccomp). A hard per-run memory ceiling via cgroup v2 `memory.max` is
not yet wired; the in-process heap watchdog (a clean per-process measure under
isolation) is the stand-in.

On top of the floor the **parent** runs a wall-clock **deadline-kill** watchdog
(`CHIDORI_ISOLATE_DEADLINE_MS`): a thread that `SIGKILL`s a wedged child that has
stopped cooperating entirely. This is the hard backstop distinct from the
in-engine `CHIDORI_JS_DEADLINE_MS`.

### Per-OS confinement (`isolate/sandbox.rs`)

Because brokering means the child needs *no* outward capability on any platform,
even a coarse per-OS sandbox is meaningfully strong — there is nothing wired for
it to abuse. Note the flip side: brokered `http` executes in the *parent*, with
the parent's network reach, so the OS sandbox alone is no defense against
server-side request forgery. That is the SSRF guard's job (`runtime::ssrf`):
the parent refuses `http` destinations that resolve to non-public addresses
(loopback, RFC 1918, the 169.254.169.254 cloud-metadata range, and their IPv6
equivalents), checked at DNS-resolution time and on every redirect hop, with
`CHIDORI_HTTP_ALLOW_HOSTS` as the deliberate allowlist.
Each layer is **best-effort**: a layer that cannot be applied (older
kernel, rootless container) logs a skip note and degrades rather than breaking
the run. Set `CHIDORI_ISOLATE_REQUIRE_SANDBOX=1` to **fail closed** if the
platform's core layer (seccomp on Linux, Seatbelt on macOS) cannot be applied.

**Linux** (`apply_linux`), layered before seccomp so `unshare`/`landlock_*`
remain legal:

- **Empty network namespace** (`unshare(CLONE_NEWNET)`) — no interfaces, so
  network egress is impossible at the OS even if a syscall slipped through. Needs
  `CAP_SYS_ADMIN`; skipped rootless.
- **Landlock read-only filesystem** (kernels ≥ 5.13) — denies every write-class
  access while leaving reads for the C runtime, closing the `openat`-write
  surface seccomp leaves open and sparing inherited fds like a redirected stderr.
- **seccomp-bpf denylist** (via `seccompiler`, safe Rust) — `NO_NEW_PRIVS` +
  `KILL_PROCESS` on a curated denylist: the whole socket family, `exec*`,
  `ptrace`/`process_vm_*`, namespace/mount, privilege-change,
  kernel-module/`bpf`/`perf_event_open`, and keyring syscalls. `fork`/`clone` are
  *not* denied (the watchdog thread needs them, and a fork that cannot `exec`
  gains no code) — the `exec*` denial is what forecloses code execution. It is a
  **denylist, not an allowlist**, deliberately: it cannot false-positive-kill the
  engine and ships real confinement today; the near-empty allowlist remains the
  end goal.

**macOS** (`apply_macos`):

- **Seatbelt** via `sandbox_init` (the deprecated-but-stable libSystem FFI
  Chromium's renderer uses) with an allow-default, targeted-deny SBPL profile
  (`(deny network*)` + `(deny file-write*)`) — the same posture as the Linux
  seccomp+Landlock pair. The feature's `unsafe` FFI (Seatbelt, `unshare`,
  `setrlimit`, `kill`) stays in the host-side `isolate/` module, out of the
  engine crate. (Type-checked on the Linux host but
  runtime-unverified — no macOS CI host yet; the best-effort design degrades a
  load failure to a logged skip.)

**Guarantee matrix:**

| Mechanism | Linux | macOS |
|---|---|---|
| Separate process + brokered effects | ✅ | ✅ |
| rlimits (CPU/FSIZE/CORE/NOFILE) | ✅ | ✅ |
| Network egress blocked at OS | ✅ empty netns | ✅ Seatbelt deny |
| Filesystem writes blocked at OS | ✅ Landlock + seccomp | ✅ Seatbelt deny |
| Syscall confinement | ✅ seccomp denylist | ⚠️ Seatbelt (coarser) |
| Hard memory ceiling | ⏳ cgroup `memory.max` (not yet wired) | ⏳ not yet wired |

Windows is not a shipped isolation target.

### Failure mapping

The parent translates an OS kill into a precise host error (the child also writes
a structured error frame from its `catch_unwind` boundary before exiting):

| Child outcome | Host error |
|---|---|
| seccomp kill (`SIGSYS`) | blocked syscall (seccomp/SIGSYS) |
| `RLIMIT_CPU` exceeded | CPU limit exceeded |
| OOM / memory kill | memory limit exceeded |
| parent deadline kill | wall-clock deadline exceeded |
| opcode budget (in-child `RangeError`) | `JavaScript exception: …` (unchanged) |
| panic → error frame, exit | `rust engine panicked: …` (unchanged shape) |
| pipe EOF / oversized frame | isolated run failed: worker terminated |

### Enabling it

Isolation is **on by default** for the CLI on Unix (Linux and macOS get an OS
sandbox layer; other Unixes get process separation + rlimits): when
`CHIDORI_ISOLATE` is unset, `chidori run`/`chidori serve` isolate each run.
Opt out with `--no-isolate` or `CHIDORI_ISOLATE=off`; `--isolate` remains as an
explicit override of an ambient `off`. Embedders of the library keep the
historical opt-in behavior (unset means off) — only the `chidori` binary flips
the default. The worker child always has the env var explicitly set to `off`
so it never recursively re-isolates. The startup banner prints an `Isolation:`
line describing the active posture. Isolation (process sandbox) and
`--untrusted` (policy) are **orthogonal but composable** — running untrusted
*without* isolation prints a nudge rather than silently changing behavior.

| Env var | Default | Effect |
|---|---|---|
| `CHIDORI_ISOLATE` | unset (on for the CLI on Unix; off for embedders) | `process` runs each agent in a confined child worker; `off` disables. Set by `--isolate` / `--no-isolate`. |
| `CHIDORI_ISOLATE_REQUIRE_SANDBOX` | off | Fail the run closed if the platform's core confinement (seccomp/Seatbelt) can't be applied. |
| `CHIDORI_ISOLATE_DEADLINE_MS` | off | Parent-side wall-clock `SIGKILL` of a wedged worker. |
| `CHIDORI_ISOLATE_CPU_SECS` | off | Hard `RLIMIT_CPU` ceiling on worker compute. |
| `CHIDORI_ISOLATE_NOFILE` | 256 | `RLIMIT_NOFILE` (clamped to the inherited hard limit). |
| `CHIDORI_ISOLATE_FSIZE_BYTES` | off | `RLIMIT_FSIZE` (opt-in; off because a `0` cap also kills a redirected regular-file stderr). |
| `CHIDORI_ISOLATE_NO_CORE` | on | Disable core dumps (`RLIMIT_CORE=0`). |

## Current gaps

These are the known limitations as of this writing. None of them are
memory-safety holes (the engine is safe Rust); they are confinement and
resource-precision gaps.

1. **The bare-CLI default is allow.** The powerful effects — `http` (real
   outbound network requests) and `workspace.*` (real disk I/O within a sanitized
   root) — all pass through the policy enforcement gate (`enforce_policy`), and the
   surface where untrusted callers actually arrive is deny-by-default:
   **`chidori serve` runs under the [`untrusted` profile](#the-untrusted-policy-profile-deny-by-default)
   unless the operator explicitly configures policy** (any valid `CHIDORI_POLICY*`
   source, or the `--trusted` flag to opt back into the permissive default;
   malformed policy configuration fails closed to deny rather than falling back to
   allow-all). What is deliberately permissive is `chidori run` without
   flags: local CLI runs of developer-authored code get the
   `AlwaysAllow` fallback, with `--untrusted` /
   `CHIDORI_POLICY_PROFILE=untrusted` available when the code being run is not
   trusted.

2. **Per-run memory accounting is thread-attributed, not ownership-attributed.**
   Each run registers a per-run meter on its execution thread
   (`src/mem_guard.rs::RunMeterGuard`), so the cap measures that run's own
   allocations and concurrent runs do not trip each other's caps. The residual
   imprecision: bytes a host effect allocates on *other* threads (e.g. tokio
   workers buffering an HTTP response) are not charged until they reach the run
   thread, and a value allocated on the run thread but freed elsewhere stays
   charged (the meter clamps at zero in the other direction). For a
   single-threaded VM run this drift is small; only true ownership accounting
   (charge at string/object allocation, credit on `Drop` inside the engine)
   would eliminate it. **Under [`--isolate`](#os-level-isolation-opt-in---isolate) this
   drift disappears**: each run is its own process, so the meter is a clean
   per-process measure with no cross-tenant attribution.

3. **Memory enforcement granularity.** The live-heap check is polled (watchdog
   every ~10 ms by default, tunable via `CHIDORI_JS_MEM_POLL_MS`; the VM observes
   the trip every 256 ops). A run can therefore overshoot the cap briefly before
   unwinding. Bounded in practice because the per-op size caps mean no single
   opcode allocates more than ~16 MB, but it is not a hard instantaneous ceiling.
   A *hard*, kernel-enforced ceiling (cgroup v2 `memory.max`) under
   [`--isolate`](#os-level-isolation-opt-in---isolate) is not yet wired — see
   gap #4.

4. **OS-level isolation is opt-in, not the default.** By default the engine runs
   in-process with the host — no seccomp, namespace, or separate-process boundary
   — so the default posture is purely capability-confinement plus Rust memory
   safety. The [`--isolate` mode](#os-level-isolation-opt-in---isolate)
   *provides* that boundary, but the operator must enable it; in-process is
   the default for trusted local dev. Sub-gaps within the isolated path:
   - **No hard memory ceiling.** cgroup v2 `memory.max` needs delegation and
     is not yet wired; `RLIMIT_AS` is too blunt for a multi-threaded VM. The polled
     heap watchdog (cleaner per-process under isolation) is the stand-in.
   - **seccomp is a denylist, not an allowlist.** Real confinement today but the
     near-empty allowlist remains the stronger end state.
   - **Rootless net-ns and mount/pid namespaces** are not yet wired (the empty
     network namespace needs `CAP_SYS_ADMIN` and is skipped rootless; the socket
     seccomp block is the rootless backstop).
   - **The macOS Seatbelt path is runtime-unverified** (type-checked only — no
     macOS CI host yet); it degrades to a logged skip on failure.

5. **Container element counts beyond arrays are uncapped.** Arrays are bounded by
   `MAX_DENSE_ARRAY` (2^25, ~33.5M), but `Map`/`Set`/object property counts are not
   individually capped. The memory ceiling (gap 2) is the backstop for the bytes
   they consume; there is no separate per-container element limit.

6. **Cycles are reclaimed only at run boundaries.** `Rc<RefCell>` cannot
   reclaim cycles mid-run. `run_module` calls `Vm::dispose()` after every run
   (`src/runtime/rust_engine.rs`), which breaks the outgoing edges of every
   object the VM allocated — including cycles disconnected from the realm
   roots — so a long-lived server thread does not leak run-over-run. A
   refcount-accounting cycle collector also exists (`Vm::collect_cycles`,
   `crates/chidori-js/src/gc.rs`) but is not wired into the run loop, so
   within a run cycles accumulate until teardown. The per-run memory cap is
   the backstop for the bytes involved. **The
   [`--isolate`](#os-level-isolation-opt-in---isolate) path sidesteps this
   entirely**: the child process exits after one run (spawn-per-run, no warm
   pool), so no state — leaked or otherwise — survives across runs.

7. **Engine maturity.** The pure-Rust engine is at 99.08% Test262 (see
   [`docs/conformance.md`](./conformance.md)); spec deviations are not
   memory-unsafe but can produce surprising behavior or, in edge cases, perturb
   determinism/replay. This is a correctness-maturity caveat, not a containment
   risk.

## The `untrusted` policy profile (deny-by-default)

The permission policy (`src/policy.rs`) gates the powerful host effects — `http`,
every `workspace:*` action (`workspace:list` / `read` / `write` / `delete` /
`manifest`), `tool:<name>` calls, and `app_data:<action>` — through
`enforce_policy` (`src/runtime/typescript/bindings.rs`). The fallback for an unmatched effect
depends on the surface: on `chidori run` (trusted, developer-authored code on the
developer's own machine) it is `AlwaysAllow`; on **`chidori serve`** — the surface
untrusted callers reach — it is **deny-by-default** unless the operator
explicitly configures policy.

The **`untrusted`** profile is a ready-made, deny-by-default policy you can select
by name — no hand-written JSON. It is what `chidori serve` runs under out of the
box; select it explicitly with a CLI flag (on `run` and `serve`) or an
environment variable:

```sh
chidori run --untrusted agent.ts                       # CLI flag
chidori serve --untrusted agent.ts                     # also on serve (already the default there)
CHIDORI_POLICY_PROFILE=untrusted chidori run agent.ts  # env-var equivalent
```

The flag is the operator's last word: `--untrusted` takes precedence over **all**
`CHIDORI_POLICY*` env vars (including a permissive `CHIDORI_POLICY_FILE` /
`CHIDORI_POLICY`), so a wrapper script can guarantee confinement regardless of
ambient configuration.

Semantics:

- **Fallback: `NeverAllow`.** Any gated effect with no matching allow-rule is
  refused with `policy: \`<target>\` denied`.
- **Allowed:** `workspace:list`, `workspace:read`, `workspace:manifest` —
  read-only introspection of the sanitized workspace root, which mutates nothing
  and cannot reach outside the root.
- **Denied:** `http` (network egress) and `workspace:write` / `workspace:delete`
  (disk mutation within the root), plus anything else that reaches the gate.

The fallback governs exactly the powerful surface, because the *pure* effects
(`log`, `template`, `memory`, `prompt`, …) never call `enforce_policy` and so run
regardless of the profile — they have no ambient authority to abuse.

Selection order is the `--untrusted` flag first, then env-driven resolution:
`CHIDORI_POLICY_FILE` → `CHIDORI_POLICY` (inline JSON) → `CHIDORI_POLICY_PROFILE`
(a built-in name) → the surface default. The surface default differs:

- **`chidori run`** falls back to the permissive profile (`AlwaysAllow`, no
  rules) — the historical default for trusted local development.
- **`chidori serve`** falls back to the `untrusted` profile, with a denial
  reason that names the opt-outs. A malformed `CHIDORI_POLICY*` source fails
  closed to this default rather than silently serving allow-all. Pass
  `--trusted` to restore the permissive `chidori run` resolution (the startup
  banner's `Policy:` line reports the active posture either way).

To customize further, copy the profile's shape into your own `CHIDORI_POLICY`
JSON (rules + `"default": "never_allow"`).

### The `supervised` profile (ask-by-default)

The **`supervised`** profile is the approval-flow sibling of `untrusted`: the same
read-only workspace allowlist, but the fallback is `AskBefore` instead of
`NeverAllow`. Under the server, a gated call suspends the run — the session
transitions to `awaitingapproval` with a `pending_approval` carrying the
`(target, args)` being asked about — and `POST /sessions/:id/approve` with
`{"decision": "allow"}` or `{"decision": "deny"}` settles it. Approvals are
remembered per `(target, args)` for the rest of the session, so the agent does not
re-ask for an identical call. On the bare CLI (where nothing can answer the
prompt) the gated call errors instead, unless `CHIDORI_POLICY_AUTO_APPROVE=1`.

Select it anywhere a profile name is accepted: `CHIDORI_POLICY_PROFILE=supervised`
or a session's `policy_profile` field (below).

### Per-session profiles over the HTTP API

A multi-tenant server can mix trusted and untrusted callers without restarting or
re-configuring: `POST /sessions` and `POST /sessions/stream` accept an optional
`policy_profile` field (alias `policyProfile`) naming a built-in profile.

```sh
curl -X POST localhost:8080/sessions \
  -d '{"input": {}, "policy_profile": "untrusted"}'
```

Semantics:

- **Stricter-wins layering.** The session profile is layered on the server
  policy and, for every gated call, the *stricter* of the two decisions applies
  (`never_allow` > `ask_before` > `always_allow`). A caller-selected profile can
  therefore tighten what the operator's policy allows but can never relax it —
  selecting a profile is not an escalation path even on a hardened server.
- **Sticky for the session's lifetime.** The profile name is persisted on the
  session and re-applied on every re-run: `input()` resume, approval replay, and
  `/replay`. It is reported back in the session JSON as `policy_profile`.
- **Validated up front.** An unknown profile name is a `400` at creation; a
  stored name that no longer resolves (e.g. after a version change) fails closed
  to `untrusted` rather than silently running under the looser server policy.

Both SDKs expose this: `client.run(input, { policyProfile: "untrusted" })` in
TypeScript, `client.run(input, policy_profile="untrusted")` in Python.

## How to harden for untrusted code

If you intend to run code you do not trust on this engine today:

1. Select the deny-by-default policy: `chidori run --untrusted` (or
   `CHIDORI_POLICY_PROFILE=untrusted`, above); `chidori serve` is already there
   by default. This denies `http` and `workspace` mutations while leaving
   read-only workspace introspection available.
2. Lower `CHIDORI_JS_OP_BUDGET` and `CHIDORI_JS_MEM_CAP_MB` to fit the workload
   (and tighten `CHIDORI_JS_MEM_POLL_MS` alongside a small cap), and enable
   `CHIDORI_JS_DEADLINE_MS` (acceptable because untrusted code should not be
   making slow trusted host calls).
3. Add OS isolation with `--isolate` (`chidori run --isolate`, `chidori serve
   --isolate`, or `CHIDORI_ISOLATE=process`): each run executes in a confined
   child process and brokers its effects back over a pipe, so a breakout has no
   ambient process to land in. Layers are best-effort — set
   `CHIDORI_ISOLATE_REQUIRE_SANDBOX=1` to fail closed if the platform's core
   confinement can't be applied. See
   [OS-level isolation](#os-level-isolation-opt-in---isolate) for the full
   posture. (Running each agent in its own container is still complementary.)
4. Keep `node:fs` on `FsPolicy::Captured` (the VFS) and avoid `workspace.*`.

## Verification

The protections above are exercised by:
- `crates/chidori-js/tests/smoke.rs::string_growth_is_bounded` — string/template
  caps throw `RangeError` rather than OOM.
- `src/runtime/rust_engine.rs::tests::run_agent_opcode_budget_terminates_infinite_loop`
  — the opcode budget terminates a runaway loop.

OS isolation (`--isolate`) is exercised by:
- `src/runtime/rust_engine.rs::tests::isolated_run_matches_in_process_byte_for_byte`
  — the worker's output **and** host-call log match the in-process path exactly.
- `crates/chidori/tests/isolate_limits.rs` (skip-aware where a layer is
  unavailable): `isolated_run_succeeds_under_the_default_resource_floor` (no false
  positives), `seccomp_blocks_a_denied_syscall` (a post-filter `socket()` probe is
  killed), `filesystem_writes_are_blocked_when_confined` (Landlock/Seatbelt),
  `parent_deadline_kills_a_wedged_worker`, `cpu_limit_terminates_a_busy_worker`,
  and `seatbelt_loads_and_enforces_on_macos`.
