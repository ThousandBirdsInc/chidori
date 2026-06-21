# OS-level isolation: process-per-run with brokered effects

> **Status:** Phase 1 implemented (`crates/chidori/src/runtime/isolate/`); phases
> 2–5 (resource floor + per-OS sandbox) not yet started.
> **Closes:** [`docs/sandbox-model.md`](./sandbox-model.md) gap #4 ("No process / OS-level
> isolation"), and as a side effect tightens gaps #2, #3, #6 (memory accounting
> precision and cross-run heap hygiene).
> **Related:** [`docs/sandbox-model.md`](./sandbox-model.md),
> [`docs/captured-effects-vfs-crypto-timers.md`](./captured-effects-vfs-crypto-timers.md),
> [`docs/running-modes.md`](./running-modes.md).

## TL;DR

Today the `chidori-js` VM runs **in-process** with the host: strong
capability-confinement + Rust memory safety, but no OS boundary
([`sandbox-model.md`](./sandbox-model.md) gap #4). This doc specifies an
**additive** isolation mode that runs the VM in a **disposable child process per
run**, with the child holding *only* the JavaScript engine. Every `chidori.*`
effect — and every captured sync native (VFS, crypto, DOM) — is **RPC'd back to
the parent over a pipe**; the parent keeps doing all real I/O (LLM, HTTP, disk,
MCP) and owns the durable call log. The child runs under a **near-empty syscall
allowlist** (Linux seccomp) / OS sandbox (macOS Seatbelt), so even a total
compromise of the interpreter cannot touch the network, the filesystem, or
another run.

Three existing design properties make this unusually cheap:

1. **The host-call boundary is already a serialization seam.** Effects flow
   through a single synchronous function — `backend.dispatch(effect, args) ->
   Result<Value, String>` (`rust_engine.rs:356`) — whose entire interface is
   `(name: &str, args: &Value) -> JSON`. That *is* the wire format. Brokering is
   a drop-in replacement of one closure.
2. **The engine has no ambient authority.** It makes essentially no syscalls of
   its own (alloc + futex), so the child's seccomp allowlist can be far tighter
   than any Node/Python sandbox.
3. **Durability is deterministic-replay, not VM-image snapshot.** Pause = kill
   the child; resume = spawn a fresh child and fast-forward through the journal
   (zero LLM calls). The child is **fully disposable** — no child-memory
   checkpointing is ever needed.

Agent code, the SDKs, the durable format, and replay semantics are **unchanged**.
Brokering is invisible above the seam.

## Why process-per-run (vs process-per-session, effects-in-child)

The sibling design (process-per-session with the host backend living *inside* the
child) needs the child to keep network/disk/MCP, so its seccomp profile must stay
wide. Process-per-run with brokered effects is strictly stronger: the untrusted
process literally cannot name a syscall that reaches the outside world, because
every powerful effect is performed on the *other* side of the pipe by trusted
code that already validates it (policy gate, workspace path sanitization, JSON
deserialization). The cost is one IPC hop per effect — negligible, since effects
are already `await`ed and dominated by LLM/tool latency.

## Architecture

```
┌─────────────────────────────── PARENT (trusted) ───────────────────────────────┐
│  chidori serve / run                                                            │
│   • transpile + resolve the full module graph (filesystem)                      │
│   • spawn + sandbox the child, set up pipe + cgroup/rlimits                      │
│   • BROKER LOOP: read (effect, args) ─► HostBindingBackend.dispatch ─► write JSON│
│        - chidori.*  : log, prompt, tool, callAgent, http, memory, template,      │
│                       checkpoint, input, workspace.*                             │
│        - __chidori_*: VFS read/write/append, crypto hash/hmac/random, dom_render │
│   • owns: durable call log / journal, policy, MCP, OTEL, captured determinism    │
│   • enforces wall-clock deadline (kill child); reaps child; maps exit → error    │
└───────────────────────────────────▲──────────────┬─────────────────────────────┘
                                     │ result JSON  │ (effect, args)
                            length-prefixed frames over a socketpair fd
                                     │              ▼
┌─────────────────────────────── CHILD (untrusted) ──────────────────────────────┐
│  chidori __run-worker   (seccomp / Seatbelt; own cgroup; rlimits; no_new_privs)  │
│   • chidori-js Engine: parse (oxc) → compile → stack VM                          │
│   • opcode budget (VM counter), regex/stack caps  ← stay in-child                │
│   • dispatch closure = blocking pipe round-trip (replaces backend.dispatch)      │
│   • fetch polyfill / node: shims / DOM build → emit __chidori_* over the pipe    │
│   • NO fs, NO net, NO clock, NO child processes — nothing is wired               │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### What moves, what stays

| Concern | Today (in-process) | Proposed (isolated) |
|---|---|---|
| JS parse / compile / execute | `run_module` (`rust_engine.rs:317`) | **child** (same code, worker entrypoint) |
| Opcode budget | VM counter (`exec.rs`) | **child** (unchanged) |
| `chidori.*` effects | `backend.dispatch` inline | **parent** via broker; child closure = pipe RTT |
| Sync natives (VFS / crypto / DOM) | `install_sync_natives` inline | **parent** via broker (state lives in `RuntimeContext`) |
| Transpile + import resolution | `transpile_module` + `load` closure (reads sibling `.ts` from disk) | **parent** ships the fully-linked graph; child never touches disk |
| Module loader `load` (`rust_engine.rs:425`) | reads files in-process | **parent**; child receives prelinked sources |
| Memory ceiling | `CountingAllocator` watchdog (`mem_guard.rs`) | **OS**: cgroup `memory.max` / `RLIMIT_AS` (hard, not polled) |
| Wall-clock deadline | watchdog trips `vm.interrupt` | **parent** kills child (or keep in-child too) |
| Panic containment | `catch_unwind` (`rust_engine.rs:449`) | child still catches, writes error frame, exits cleanly |
| Durable call log / replay / policy / MCP / OTEL | parent (`HostBindingBackend`) | **parent** (unchanged) |

Note the bonus: moving the JS heap into its own process replaces the *polled,
thread-attributed* memory meter with a *hard, kernel-enforced* per-run ceiling —
which is exactly what gaps #2 and #3 of `sandbox-model.md` ask for. And because
the child exits after one run, the `Rc<RefCell>` cycle-leak concern across runs
(gap #6) disappears for the isolated path.

### The seam, concretely

`run_module` installs effects with:

```rust
let dispatch: Rc<dyn Fn(&str, &Value) -> Result<Value, String>> =
    Rc::new(move |effect, args| backend.dispatch(effect, args));
engine.install_chidori_effects(dispatch);                 // rust_engine.rs:356
engine.install_sync_natives(SYNC_NATIVE_NAMES, sync);     // rust_engine.rs:348
```

In the worker, `backend.dispatch` (and the sync-native dispatch) are replaced by
a blocking call that writes a request frame and reads the response frame:

```rust
// worker side — same Fn signature, so install_chidori_effects is untouched
let broker = PipeBroker::from_fd(effect_fd);
let dispatch = Rc::new(move |effect: &str, args: &Value| {
    broker.call(effect, args)   // write (effect,args); block; read Result<Value,String>
});
```

Because the engine already drives effects synchronously on the run thread (it
blocks in `dispatch` today), the child blocking on a pipe read is semantically
identical — no change to the VM, the SDK, or determinism.

On the **parent** side the broker is one task per run:

```rust
loop {
    let (effect, args) = read_frame(&mut pipe)?;           // untrusted bytes
    let result = backend.dispatch(&effect, &args);         // existing validated path
    write_frame(&mut pipe, &result)?;
}
```

`backend.dispatch` is *already* the trust boundary — it enforces policy, sanitizes
workspace paths, and deserializes JSON into typed handlers (`bindings.rs:841`).
Brokering does not widen it; it makes it a process boundary too.

### `callAgent` and nested tools

Today a sub-agent / TS tool re-enters `run_module` on the same engine, sharing the
backend so its effects nest under the parent (`run_tool_file` /
`run_agent_file`, `rust_engine.rs:141`). Under isolation, `callAgent` and a TS
`tool` become **brokered effects the parent satisfies by spawning a nested child**
(process-per-run, recursively), threading the same `parent_seq` so nesting,
journaling, and OTEL spans are preserved. Depth is bounded by the existing call
depth. MCP tools already run as external processes (`mcp/client.rs`) — unchanged.

### Pause / resume / replay

`chidori.input()` and signals already suspend by **unwinding** with `PAUSE_MARKER`
and resuming via a *fresh engine that replays the journal to the pause point*
(`rust_engine.rs:109`, `host.rs`). That maps onto process-per-run for free:

- **Pause** → the child returns the pause sentinel and exits; the parent persists
  as it does today.
- **Resume / replay / branch** → the parent spawns a fresh worker and the journal
  is replayed (the parent serves recorded effect results over the same pipe; the
  child cannot tell record from replay). **Zero** child-state serialization.

## Wire protocol

A minimal, length-prefixed framing over a single `socketpair(2)` fd (Unix) /
anonymous pipe (Windows). Keep it dumb so the child's seccomp allowlist stays at
`read`/`write`/`futex`/`mmap`/`exit`.

- Frame = `u32 length` (LE) + body. Body is `postcard`/`bincode` (compact, safe
  Rust) or JSON; recommend a binary codec to avoid re-stringifying large blobs.
- Request: `{ effect: String, args: Value }`. Response: `{ ok: Value } | { err: String }`.
- **Parent hardening:** enforce a max frame size (reject → kill the run) so a
  hostile child cannot OOM the parent with a giant `args`; treat EOF / decode
  error / child death as a run failure; never block past the run deadline on a
  read.
- A second control channel (or a tagged frame kind) carries the initial
  `{ bundle, prelinked_modules, input, limits }` handoff and the final
  `{ result | error }`.

Throughput: a pipe round-trip is microseconds; effects are ms–seconds. The only
chatty surface is captured sync natives (VFS reads, crypto) inside hot loops — if
profiling shows it, ship a **read-only VFS snapshot** into the child (writes still
broker) as a later optimization. Not needed for v1.

## Cross-platform isolation

Brokering means the child needs *no* outward capability on **any** platform, so
even a weak per-OS sandbox is meaningfully strong (there is nothing wired for it
to abuse). We tier the **kernel-enforced** guarantee and keep a common floor.

### Common floor (all platforms)

- **Separate process**, no inherited fds except the broker socket + stderr.
- **rlimits**: `RLIMIT_AS` (address space), `RLIMIT_CPU`, `RLIMIT_NOFILE` (tiny),
  `RLIMIT_NPROC` (0–1, block `fork`), `RLIMIT_FSIZE = 0`, core dumps off.
- **`no_new_privs`** equivalent; clear the environment; chdir to an empty dir.
- A `trait Sandbox { fn pre_exec(&self) -> io::Result<()>; }` with per-OS impls;
  the parent supervisor and worker entry are platform-agnostic above it.

### Tier 1 — Linux (primary target)

Strongest guarantee, no daemon required on modern kernels:

- **seccomp-bpf** allowlist via the `seccompiler` crate (safe Rust, from the
  Firecracker project — fits the engine's zero-`unsafe` ethos). Allowed:
  `read, write, close, mmap, munmap, mremap, brk, madvise, futex,
  rt_sigreturn, rt_sigaction, rt_sigprocmask, sched_yield, exit, exit_group`,
  plus `clock_gettime`/`getrandom` if std needs them (see notes). Default action
  `SECCOMP_RET_KILL_PROCESS` in production (`RET_ERRNO(EPERM)` + log in dev).
- **Namespaces** (unprivileged user namespace as the entry): `user, mount, pid,
  net (empty — no interfaces), ipc, uts, cgroup`. An empty net namespace makes
  network egress impossible even if a syscall slipped through.
- **cgroup v2** `memory.max` (hard per-run heap ceiling — replaces the polled
  watchdog), `pids.max`, optional `cpu.max`. Delegation may need systemd/root;
  document the unprivileged fallback (rlimits-only) when cgroup delegation is
  unavailable.
- **Landlock** (via the `landlock` crate) as defense-in-depth on the filesystem
  for kernels ≥ 5.13, even though brokering already means the child opens nothing.
- Crates: `seccompiler`, `landlock`, `rustix`/`nix` (clone/unshare/rlimit),
  `cgroups-rs` or direct cgroupfs writes.

**Notes / decisions to nail down:** Rust's `HashMap` seeds `RandomState` via
`getrandom` at startup — either allow `getrandom` (a capability-free
non-determinism source the engine never exposes to JS) or pre-seed before
sandbox lock-in. JS `Date`/`performance.now`/`Math.random` are already
virtualized through captured effects, so the child needs no real clock or RNG of
its own.

### Tier 2 — macOS (parity target; shipped binary)

- **Seatbelt** via `sandbox_init()` with a `(deny default)` SBPL profile that
  allows only the broker fd and process basics — the same mechanism Chrome's
  renderer uses. The API is deprecated-but-present and stable; isolate the one
  bit of `libc` FFI behind the `Sandbox` trait (the only `unsafe` in the
  feature, kept out of the engine crate).
- **rlimits** as above; `posix_spawn` with a closed-fd policy.
- Evaluate the `birdcage` crate (cross-platform Landlock+seccomp / Seatbelt
  wrapper) to share code — but it targets "allow these paths/hosts," whereas our
  child wants "allow almost nothing," so a hand-rolled minimal profile is likely
  cleaner. Decide during the spike.

### Tier 3 — Windows (not a shipped target today)

Documented for completeness; implement only if Windows becomes a target. Job
Object (kill-on-job-close, `JOB_OBJECT_LIMIT_PROCESS_MEMORY`, active-process cap)
+ a restricted/low-integrity token or AppContainer. No syscall-filter equivalent;
the guarantee is process isolation + resource caps + restricted token, leaning on
brokering for the rest. Mark clearly as the weakest tier.

### Guarantee matrix

| Mechanism | Linux | macOS | Windows |
|---|---|---|---|
| Separate process + closed fds | ✅ | ✅ | ✅ |
| rlimits (AS/CPU/NPROC/FSIZE) | ✅ | ✅ | partial (Job Object) |
| Hard memory ceiling | cgroup `memory.max` | `RLIMIT_AS` | Job Object |
| Syscall allowlist | ✅ seccomp | ⚠️ Seatbelt (coarser) | ❌ |
| Network egress blocked at OS | ✅ empty netns | ✅ Seatbelt deny | ⚠️ token/firewall |
| Filesystem blocked at OS | ✅ mount ns + Landlock | ✅ Seatbelt deny | ⚠️ token |

## Failure mapping

The parent translates child exit into a host error, surfacing the structured
error frame the child writes from its `catch_unwind` boundary before exiting:

| Child outcome | Host error |
|---|---|
| seccomp kill (`SIGSYS`) | `sandbox violation: disallowed syscall` |
| cgroup OOM / `RLIMIT_AS` | `memory limit exceeded` |
| parent deadline kill | `wall-clock deadline exceeded` |
| opcode budget (in-child `RangeError`) | `JavaScript exception: …` (unchanged) |
| panic → error frame, exit | `rust engine panicked: …` (unchanged shape) |
| pipe EOF / oversized frame | `isolated run failed: worker terminated` |

## Configuration & rollout

- **Off by default**; in-process stays the default for trusted local dev.
- New worker subcommand `chidori __run-worker` (hidden), added to the clap
  `Commands` enum (`main.rs:44`); the parent `exec`s its own binary in this mode.
- Opt-in: `--isolate` flag on `run`/`serve` and `CHIDORI_ISOLATE=process` (env).
  Naturally pairs with `--untrusted` — consider having the `untrusted` /
  `supervised` policy profiles *imply* isolation when the platform supports it.
- The server already runs each request under `tokio::task::spawn_blocking`
  (`server.rs:977` et al.); the broker loop slots in there as one task per run,
  so concurrency = many children + many broker tasks, exactly as today.
- **Spawn-per-run by default** (clean, disposable, leak-free — and it resolves
  gaps #2/#3/#6 for the isolated path). An optional **warm worker pool** (fork a
  pre-sandboxed worker, hand it a bundle per run) is a latency optimization for
  high-throughput servers, but only with a proven cross-run reset; defer to v2.

## Phasing

1. **Worker mode + broker, no sandbox.** ✅ **Done** —
   `crates/chidori/src/runtime/isolate/` (`protocol`, `worker`, `supervisor`).
   `run_module` now routes every host op through a single `RunHost` seam
   (`InProcessHost` in-process; `BrokeredHost` in the worker); `chidori run
   --isolate` / `CHIDORI_ISOLATE=process` spawns `chidori __run-worker` and
   brokers `chidori.*` effects, captured natives, DOM flushes, and module loads
   over stdin/stdout. Parity is asserted byte-for-byte (output **and** host-call
   log) against the in-process path
   (`rust_engine::tests::isolated_run_matches_in_process_byte_for_byte`). No
   sandbox yet — the child is a separate process with brokered effects only.
2. **Resource floor.** rlimits + cgroup `memory.max`; move the memory ceiling off
   the in-process watchdog for the isolated path; deadline-kill from the parent.
3. **Linux syscall confinement.** seccomp allowlist + empty netns + mount ns +
   Landlock; negative tests (a worker that calls `socket()`/`open("/etc/passwd")`
   is killed).
4. **macOS Seatbelt** profile + the `Sandbox` trait FFI; CI parity.
5. **Polish:** `--isolate` UX, policy-profile coupling, docs, optional warm pool.

## Verification

- **Parity:** every existing rust-engine test passes through the worker with
  identical output (drive the suite in both modes behind a test flag).
- **Protocol:** frame round-trip; oversized-frame rejection; child-crash → run
  error; deadline interrupts a blocked read.
- **Confinement (negative):** a worker bundle that attempts a raw `socket`,
  `connect`, `open` of a real path, or `fork` is killed by seccomp/Seatbelt; an
  agent that tries `node:fs` host mode still hits the VFS (brokered) and never
  real disk; network is blocked at the OS even with a deliberately broken policy.
- **Resource:** Map-of-8MB-strings agent is OOM-killed by cgroup at a 64 MB cap
  and returns `memory limit exceeded`; `while(true){}` still terminates via the
  in-child opcode budget; a long sleep trips the parent deadline.
- **CI matrix:** Linux (full tiers) + macOS (Seatbelt) on the existing targets.

## Open questions

- **getrandom / clock at startup:** allow narrowly, or pre-seed and forbid? (Lean
  pre-seed for the tightest profile; measure std's needs first.)
- **cgroup without root:** require systemd delegation, or ship rlimits-only as the
  unprivileged fallback and document the reduced guarantee?
- **birdcage vs hand-rolled:** adopt for macOS+Linux to cut code, or keep a
  minimal hand-rolled profile for the near-empty allowlist? (Spike both.)
- **Sync-native chattiness:** is a read-only VFS snapshot in-child worth it, or do
  real agents make few enough captured fs/crypto calls that brokering them is
  free? (Profile before optimizing.)
- **Default coupling:** should `--untrusted` auto-enable `--isolate` where
  supported, or keep them orthogonal so the operator opts in explicitly?
