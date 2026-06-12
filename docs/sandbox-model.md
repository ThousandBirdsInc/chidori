# Sandbox model of the chidori-js runtime

> **Status:** Implemented, with documented gaps (see [Current gaps](#current-gaps)).
> **Target engine:** the pure-Rust `chidori-js` engine — the only JS engine in
> the tree (the QuickJS/C path was removed in #39).
> **Related:** [`docs/pure-rust-js-engine-plan.md`](./pure-rust-js-engine-plan.md),
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

What it is **not**: a containment boundary for the powerful effects the host
*does* inject (`http`, `workspace.*`). Those are real
capabilities; whether granting them is "safe" depends entirely on whether the
agent code is trusted. There is also no process/OS isolation — the engine runs
in-process with the host.

## Threat model

The framework's primary model is **trusted, developer-authored agent code**. The
"sandbox" exists chiefly to make execution *deterministic and replayable* (route
all non-determinism through captured effects) and to provide *defense-in-depth*
against bugs and accidental resource exhaustion — see the product framing in
[`docs/conformance.md`](./conformance.md) ("Bun/Node language behavior **plus** our
security sandbox and deterministic replay").

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
| Break out of the process / OS | ❌ No process or OS isolation |

## Architecture: capability injection, not ambient authority

The pure engine (`crates/chidori-js`) is a parser (`oxc`) → bytecode compiler →
stack VM with `Rc<RefCell>` reference counting. Two properties make it a sandbox:

1. **Memory safety.** There is **zero `unsafe`** in the engine crate (the only
   occurrence is a doc comment in `host.rs` describing the *old* QuickJS FFI). The
   whole stack is safe Rust plus `oxc`. The worst an interpreter bug can do is
   panic or misbehave — it cannot corrupt memory or jump into host code. This is a
   categorical improvement over embedding a C/C++ engine (QuickJS, V8) in-process.

2. **No ambient authority.** The engine contains no `std::fs`, `std::net`,
   `std::process`, `std::thread`, `Command`, or HTTP client. A bare
   `Engine::new()` running arbitrary JS can compute and allocate — nothing else.
   I/O exists **only** because the host installs it:
   - `Engine::install_chidori_effects` (`crates/chidori-js/src/lib.rs`) wires the
     async `chidori.*` effect surface (`log`, `tool`, `prompt`, `input`, `http`,
     `memory`, `template`, `checkpoint`, `callAgent`, `workspace.*`). The
     `execJs`/`execPython`/`execWasm` JS stubs remain defined but are inert — the
     host backend rejects the effect (`… is not supported on the rust engine`)
     since the snippet sandboxes were removed in #39.
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
workspace root with `..`/absolute-path rejection and symlink-traversal checks (host
side, in the TypeScript bindings). It is a deliberate capability, not an isolated
surface.

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
   `MAX_STRING_LEN` (16 MB, `crates/chidori-js/src/value.rs`). This closes the
   exponential `s += s` / `` s = `${s}${s}` `` OOM — previously unbounded, now
   capped — and matches the existing caps on `repeat`/`padStart`/`padEnd` and on
   dense-array allocation (`MAX_DENSE_ARRAY` = 1M). With these caps, no *single*
   opcode can allocate without bound.

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
| String length | (compile constant) | 16 MB | — |
| Dense array length | (compile constant) | 1,000,000 | — |
| Call depth | (compile constant) | 2,000 | — |
| Regex steps | (compile constant) | 100,000 | — |

## Current gaps

These are the known limitations as of this writing. None of them are
memory-safety holes (the engine is safe Rust); they are confinement and
resource-precision gaps.

1. **The bare-CLI default is still allow.** The powerful effects — `http` (real
   outbound network requests) and `workspace.*` (real disk I/O within a sanitized
   root) — all pass through the policy enforcement gate (`enforce_policy`), and the
   surface where untrusted callers actually arrive is now deny-by-default:
   **`chidori serve` runs under the [`untrusted` profile](#the-untrusted-policy-profile-deny-by-default)
   unless the operator explicitly configures policy** (any valid `CHIDORI_POLICY*`
   source, or the `--trusted` flag to opt back into the permissive default;
   malformed policy configuration fails closed to deny rather than falling back to
   allow-all). What deliberately remains permissive is `chidori run` without
   flags: local CLI runs of developer-authored code keep the historical
   `AlwaysAllow` fallback, with `--untrusted` /
   `CHIDORI_POLICY_PROFILE=untrusted` available when the code being run is not
   trusted.

2. **Per-run memory accounting is thread-attributed, not ownership-attributed.**
   Each run registers a per-run meter on its execution thread
   (`src/mem_guard.rs::RunMeterGuard`), so the cap measures that run's own
   allocations and concurrent runs no longer trip each other's caps. The residual
   imprecision: bytes a host effect allocates on *other* threads (e.g. tokio
   workers buffering an HTTP response) are not charged until they reach the run
   thread, and a value allocated on the run thread but freed elsewhere stays
   charged (the meter clamps at zero in the other direction). For a
   single-threaded VM run this drift is small; true ownership accounting (charge
   at string/object allocation, credit on `Drop` inside the engine) remains the
   eventual fix.

3. **Memory enforcement granularity.** The live-heap check is polled (watchdog
   every ~10 ms by default, tunable via `CHIDORI_JS_MEM_POLL_MS`; the VM observes
   the trip every 256 ops). A run can therefore overshoot the cap briefly before
   unwinding. Bounded in practice because the per-op size caps mean no single
   opcode allocates more than ~16 MB, but it is not a hard instantaneous ceiling.

4. **No process / OS-level isolation.** The engine runs in-process with the host;
   there is no seccomp, namespace, or separate-process boundary. Isolation is
   purely capability-confinement plus Rust memory safety. A panic is contained
   (gap-free via `catch_unwind`), but this is not a substitute for OS containment
   when running genuinely hostile code.

5. **Container element counts beyond arrays are uncapped.** Arrays are bounded by
   `MAX_DENSE_ARRAY` (1M), but `Map`/`Set`/object property counts are not
   individually capped. The memory ceiling (gap 2) is the backstop for the bytes
   they consume; there is no separate per-container element limit.

6. **Reference-counting GC leaks cycles within a run.** `Rc<RefCell>` cannot
   reclaim cycles; `Vm::dispose()` exists to break the realm's known cycles but
   `run_module` builds a fresh engine per run and relies on process/run teardown
   rather than calling it. Long-lived worker threads that reuse state could
   accumulate leaked bytes across runs; the baseline-relative memory cap is robust
   to this within a run but not across many runs on a reused thread.

7. **Engine maturity.** The pure-Rust engine is at 98.10% Test262 (see
   [`docs/conformance.md`](./conformance.md)); spec deviations are not
   memory-unsafe but can produce surprising behavior or, in edge cases, perturb
   determinism/replay. This is a correctness-maturity caveat, not a containment
   risk.

## The `untrusted` policy profile (deny-by-default)

The permission policy (`src/policy.rs`) gates the powerful host effects — `http`
and every `workspace:*` action (`workspace:list` / `read` / `write` / `delete` /
`manifest`) — through `enforce_policy`. The fallback for an unmatched effect
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
3. Run each agent in its own process (or container) so the lack of OS isolation
   (gap 4) does not allow a true breakout; per-run memory accounting (gap 2)
   already keeps co-tenant caps separate within one process.
4. Keep `node:fs` on `FsPolicy::Captured` (the VFS) and avoid `workspace.*`.

## Verification

The protections above are exercised by:
- `crates/chidori-js/tests/smoke.rs::string_growth_is_bounded` — string/template
  caps throw `RangeError` rather than OOM.
- `src/runtime/rust_engine.rs::tests::run_agent_opcode_budget_terminates_infinite_loop`
  — the opcode budget terminates a runaway loop.
- Manual end-to-end (rust engine + global allocator active): a Map-of-8 MB-strings
  agent trips at a 64 MB cap (`execution interrupted`) yet completes under a
  generous cap; an infinite loop trips a 500 ms deadline.
