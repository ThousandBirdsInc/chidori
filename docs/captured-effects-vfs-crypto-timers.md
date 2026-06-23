# Captured Effects: Virtual Filesystem, Crypto, and Timers

## Implementation status (Phases 1–4 landed)

This design is implemented. It originally shipped on the QuickJS path; the
pure-Rust `chidori-js` engine now runs the same surface (`node:` crypto/fs/timers,
`TextEncoder`, Web Crypto, virtual timers), reusing the same shim sources and
polyfills and capturing through the same `RuntimeContext` call log + VFS — so a
captured-effects agent records and replays identically on either engine. See the
G3 section of `docs/rust-engine-quickjs-removal-gaps.md` for the rust-path wiring.

What shipped, by phase:

- **Phase 1 — policy + capability ledger.** `RuntimePolicy` gained `fs`,
  `crypto`, `timers` fields (`FsPolicy`/`CryptoPolicy`/`TimerPolicy` in
  `src/runtime/snapshot.rs`), all `#[serde(default)]` for back-compat, durable
  default `Captured`/`Captured`/`Virtual`, and `ensure_durable_safe` rejects the
  `Host` variants. `src/runtime/capability.rs` holds the `Capability` enum and
  `CapabilityLedger`; `RuntimeContext::note_capability` raises flags and mirrors
  them to the OTEL run span (`RunSpan::record_capability`). The ledger is
  emitted on `SnapshotManifest.capabilities`.
- **Phase 2 — VFS.** `src/runtime/vfs.rs` is a `BTreeMap`-backed,
  snapshot-resident tree (base64-serialized bytes, logical mtimes). It rides
  `SnapshotManifest.vfs` and is restored on resume via
  `with_replay_host_promises_and_vfs` (wired at the two server resume sites).
  `node:fs` + `node:fs/promises` shims call `__chidori_fs_*` natives. VFS ops
  are **not** call-logged (state rides the snapshot); they only raise `Fs*`
  flags. Host seed via `CHIDORI_VFS_SEED`.
- **Phase 3 — crypto.** `globalThis.crypto` (prelude) + `node:crypto` shim.
  Hashing/HMAC (`src/runtime/crypto.rs`) run inline, flagged `CryptoHash`, never
  logged. Randomness is captured: recorded into the call log as `crypto.random`
  and replayed verbatim (`CryptoPolicy::Seeded` derives from the run seed;
  `Captured` draws the host CSPRNG; both record). Flagged `CryptoRandom`.
- **Phase 4 — timers.** A deterministic virtual timer queue (prelude,
  `TIMER_VIRTUAL_POLYFILL`) fires `setTimeout`/`setInterval`/`setImmediate` in
  `(deadline, id)` order via a self-rescheduling microtask pump, so timers run
  inside the engine's existing job drain with no real sleeping. A logical clock
  (`globalThis.__chidori_now`) drives `Date.now()` under `DatePolicy::Fixed`.
  Flagged `Timer`/`Microtask`. `timers=disabled` throws.

Deviations and enabling work not in the original design:

- **Bundler default import/export.** The snapshot bundler only supported named
  / namespace / type-only imports and named exports. `import fs from "node:fs"`
  and `export default …` are now supported (`binding_statement` /
  `export_statement`, keyed on a module-namespace `.default`). This was a
  prerequisite — the pre-existing `process`/`buffer`/`util` shims used
  `export default` and so were never actually runtime-loadable.
- **Encoding + Buffer correctness.** Added `TextEncoder`/`TextDecoder`/
  `atob`/`btoa` (`TEXT_ENCODING_POLYFILL`) — the QuickJS runtime ships none, and
  `node:buffer`/`node:fs` need them. The `node:buffer` shim's `toString`/`from`
  now handle `utf8`/`hex`/`base64`/`latin1`/`ascii` (previously only base64 +
  UTF-8 decode, which corrupted binary and could emit lone surrogates).
- **Builtin paths are project-root-independent.** `node:` builtins resolve to a
  fixed `/__node_builtins__/<name>.js` so a shim importing another builtin
  (e.g. `fs` → `buffer`) resolves consistently.
- **VFS and `chidori.workspace` stay separate stores.** The design floated
  routing `node:fs` through the workspace backing store. They are kept distinct:
  `chidori.workspace` is host-mediated real disk, while the VFS is an in-memory
  sandbox. Unifying them would force either the VFS onto real disk (breaking
  determinism) or workspace into memory (breaking its real-disk contract), so
  the captured-effects VFS is its own snapshot-resident store.
- **Capability gating is visibility-only (v1).** Flags are surfaced on the
  manifest and OTEL spans but never block; approval gating on a flag (e.g.
  `CryptoKeygen`) is left as opt-in future policy, per the Open Questions.

**Cross-suspend timers — resolved, no extra machinery needed.** Tracing the
pump showed there is effectively never a timer *pending at* a suspend point:
timers fire on the logical clock with no real delay, so the self-rescheduling
microtask pump drains the entire timer queue to empty during `drain_jobs`
*before* any host-operation block returns control. Both resume paths are
therefore correct without serializing timer state:

- **Call-log replay** re-runs the agent from the top; deterministic execution
  reschedules and re-fires the same timers while host calls replay from the
  log. Validated by `timers_coexist_with_recorded_host_calls_and_replay_identically`.
- **Live-VM resume** restores the heap (the pump closure and its task array) and
  the pending microtask queue (the fork's `snapshot_microtask_queue` /
  `restore_microtask_queue`), so any in-flight pump continuation survives.

JS timer callbacks are closures and so are intrinsically *not* serializable into
the JSON manifest — which is also why the manifest path relies on re-execution
rather than carrying the queue. `PendingHostOperationKind::Timer` remains
reserved for a hypothetical future where a timer's *effect* (not its callback)
must be modeled as a host operation; it is unused today.

## Purpose

Today the Chidori snapshot runtime makes nondeterministic and host-reaching
JavaScript surfaces *unavailable*. `snapshot_policy_prelude` (`src/runtime/snapshot.rs`)
hard-disables `WeakRef`, `FinalizationRegistry`, `SharedArrayBuffer`, and
`Atomics`; freezes `Date` to epoch 0; seeds or disables `Math.random`; and the
`node:` resolver (`src/runtime/typescript/{resolver,builtins}.rs`) allowlists a
fixed set of builtins — at the time of writing `process`, `buffer`, `util`,
`fs`, `fs/promises`, `crypto`, `http`, `https`, plus the pure-logic /
virtualized modules `path` (and `path/posix`), `events`, `url`, `assert` (and
`assert/strict`), and `os`. The pure modules (`path`/`events`/`url`/`assert`)
are deterministic by construction; `os` returns fixed virtualized constants in
the same spirit as `process.platform`. Anything outside the allowlist throws at
first use. The authoritative list is `NODE_BUILTIN_ALLOWLIST` (`transpile.rs`),
kept in sync with `BUILTIN_NAMES` (`builtins.rs`).

This document specifies the inverse policy for a specific class of surfaces:
**filesystem, crypto, and timers**. Rather than rejecting them, the runtime
will *provide* them, but every nondeterministic or host-reaching operation is
routed through a **captured effect** — recorded into the call log on first run,
replayed deterministically on resume, and surfaced as a **capability flag** on
the run manifest so operators can see exactly what an agent touched.

The guiding rule the user asked for:

> Anything we would otherwise reject must be supported, but its use must be
> flagged and the effect captured.

## Background: machinery we already have

The capture/replay pipeline this design needs already exists for host calls
(`prompt`, `tool`, `http`, `memory`, …). We extend it rather than inventing a
parallel path.

- **`CallRecord`** (`src/runtime/call_log.rs`) — `{ seq, parent_seq, function,
  args, result, duration_ms, timestamp, error }`. The ordered `CallLog` is the
  replay source of truth.
- **`RuntimeContext`** (`src/runtime/context.rs`) — owns `next_seq()`,
  `try_replay_checked(seq, function)`, and `record_call(...)`. A call either
  replays from `replay_log` by `seq` or executes live and is recorded.
- **`HostPromiseTable` / `PendingHostOperation` / `PendingHostOperationKind`**
  (`src/runtime/snapshot.rs`) — models async host operations that can suspend
  across a snapshot/restore boundary. Effects that must survive a process
  restart while pending (notably timers) use this table.
- **`RuntimePolicy`** (`src/runtime/snapshot.rs`) — `{ typescript_imports,
  date, random, maps_sets, deterministic_seed }`, serialized into the snapshot
  and checked on resume via `ensure_compatible`. Policy is the knob that
  switches a surface between `Disabled`, deterministic, and captured.
- **`snapshot_policy_prelude`** — the JS prelude string that installs/curtails
  globals. New globals (`crypto`, timer functions, `node:fs` shims) are
  installed here or via the `node:` builtin shim layer.
- **The host-call bridge** — JS pushes onto `globalThis.__chidori_host_calls`
  and awaits a host-resolved promise via `__chidori_host_method_queues`
  (`src/runtime/snapshot.rs` ~line 2937). Captured effects reuse this exact
  bridge so they nest correctly under parent calls (`parent_seq`) and stream
  through the existing engine pump.

## Goals

- Provide working `node:fs` / `node:fs/promises`, `node:crypto` /
  `globalThis.crypto`, and `setTimeout`/`setInterval`/`setImmediate` (+ their
  clears) and `queueMicrotask` to agent code.
- Every nondeterministic operation is **captured**: recorded on first
  execution, replayed by `seq` on resume, byte-for-byte identical.
- Every use of a captured surface raises a **capability flag** recorded on the
  run and surfaced in the snapshot manifest, regardless of whether the
  individual call was deterministic.
- Filesystem state is **snapshot-resident**: it survives suspend → restore and
  is identical on replay.
- No surface silently reaches the host OS. Real disk, real RNG, real wall-clock
  are reachable *only* through explicit, policy-gated, recorded effects.
- Policy-driven: each surface has `Disabled` | `Captured` | `Host` modes mirror
  the existing `DatePolicy` / `RandomPolicy` shape, with `Captured` the durable
  default.

## Non-Goals

- No `node:net`, `node:child_process`, `node:worker_threads`, `node:dns`, or
  `node:http(s)` *server* sockets in this work. These stay rejected for now.
- Outbound HTTP is served by the first-class captured host op (`http`), reached
  through an internal `globalThis.__chidori_http` native. There is **no** public
  `chidori.http`: the stdlib net surface *is* the public API. `globalThis.fetch`
  (+ `Headers`/`Request`/`Response`) and the `node:http`/`node:https` *client*
  APIs (`request`/`get`) all route through `__chidori_http`, so every network
  call — including ones made inside a dependency — inherits its security-policy
  enforcement (allow / ask / deny), the approval-pause path, and record/replay.
  See `FETCH_POLYFILL` in `runtime::typescript::helpers` (installed by the rust
  engine after `install_chidori_effects`) and the `HTTP_SHIM`/`HTTPS_SHIM` in
  `runtime::typescript::builtins`.
- No real concurrent wall-clock scheduling. Timers are virtualized against a
  logical clock (see Timers); we do not run a real OS timer wheel.
- No POSIX completeness for the VFS (no permissions bits enforcement, symlink
  loops resolution edge cases, `fs.watch`, file locks). We cover the surface
  real packages touch and throw clearly on the rest, matching the
  `builtins.rs` philosophy.
- No streaming `fs` handles serialized across snapshot (`createReadStream`
  open across a suspend). Streams are buffered/eager in v1.

## Core concept: the Captured Effect

A **captured effect** is any operation whose result is not a pure function of
in-VM state. It is mediated by a single helper shape on top of the existing
context:

```
seq = ctx.next_seq()
if let Some(record) = ctx.try_replay_checked(seq, function)? {
    return record.result          // deterministic replay, no live side effect
}
let (result, flags) = execute_live(args)?;   // the only place real host access happens
ctx.record_call(CallRecord { seq, function, args, result, .. });
ctx.note_capabilities(flags);     // raise capability flags
return result
```

Three categories, because not everything that *looks* nondeterministic is:

1. **Pure-in-VM** — e.g. `crypto.createHash('sha256').update(buf).digest()`,
   reading a file already present in the VFS. Deterministic given VM state.
   These run **inline, not captured** for performance, but **still raise a
   capability flag** ("crypto used", "fs read") so the surface is visible.
2. **Nondeterministic, capturable now** — `crypto.randomBytes`, `randomUUID`,
   reading the *initial* contents of a host-seeded file, `Date.now()` when the
   policy allows host time. Recorded into the call log on first run, replayed
   thereafter.
3. **Async + suspendable** — `setTimeout` callbacks that may straddle a
   snapshot boundary. Modeled as a `PendingHostOperation` in the
   `HostPromiseTable` so a pending timer survives restore.

### Capability flags

Add a capability ledger to `RuntimeContext`:

```rust
pub enum Capability {
    FsRead, FsWrite, FsDelete,
    CryptoHash, CryptoRandom, CryptoKeygen,
    Timer, Microtask,
    // future: Net, Subprocess, …
}
```

`note_capabilities` accumulates a `BTreeSet<Capability>` with first-touch
`seq`/`timestamp`. It is:

- Emitted on the **snapshot manifest** (alongside `RuntimePolicy`) so a stored
  run advertises its capability surface.
- Emitted as OTEL span attributes via the span emitter
  (`crates/chidori/src/runtime/otel.rs`) — one attribute per capability, e.g.
  `chidori.capability.crypto_random=true`.
- Returnable to the host (CLI/server) so a run can be gated: "this agent used
  `CryptoRandom`; require approval" reuses the `PolicyApproval` pause path.

A capability flag is **monotonic and advisory for visibility**; it never blocks
inline. Blocking, when desired, is a separate policy decision layered on top
(reuse `PendingApproval`).

## Subsystem 1 — Virtual Filesystem (VFS)

### Model

An in-memory tree owned by the runtime and **part of snapshot state**, not the
heap. A `Vfs` struct lives next to the call log in `RuntimeContextInner`:

```rust
struct Vfs {
    nodes: BTreeMap<VfsPath, VfsNode>,   // BTreeMap → deterministic iteration
}
enum VfsNode {
    File { bytes: Vec<u8>, mtime_seq: u64 },
    Dir,
}
```

- Paths are normalized (the `normalize_path` logic already in `resolver.rs`
  generalizes here) and rooted at a virtual `/`. No path escapes the VFS root;
  `..` past root clamps, it does not reach host disk.
- `mtime`/`atime` are **logical** — derived from the effect `seq`, not
  wall-clock — so stat output is deterministic. Times render through the same
  fixed/seeded clock as `Date` (see Timers).
- Iteration order is `BTreeMap` order so `readdir` is stable across runs.

### Seeding from host

The host may pre-populate the VFS before a run (analogous to how
`CHIDORI_AGENT_ENV` seeds `process.env`):

- A `CHIDORI_VFS_SEED` channel (manifest path → bytes, or a tarball pointer)
  loads files at construction. This is the **only** host-disk touch, it happens
  before agent code runs, and the seed manifest hash is recorded into the
  snapshot so replay validates it didn't change. This mirrors
  `chidori_agent_env_json()`.
- The existing `chidori.workspace` bridge (`globalThis.__chidori_workspace_*`,
  `snapshot.rs` ~line 127) is the precedent: workspace is already a
  host-mediated file surface. The VFS is its in-snapshot generalization;
  `node:fs` calls should route to the same backing store as
  `chidori.workspace` so the two views stay consistent.

### Snapshot integration

The VFS serializes with the snapshot. Because it is plain data
(`BTreeMap<String, VfsNode>`), it serializes deterministically and restores
identically. On resume:

- A read of a file present in the VFS is **pure-in-VM** → inline, flagged
  `FsRead`, not call-logged (the bytes are already in restored state).
- A write mutates VFS state and is flagged `FsWrite`. Writes are **not** host
  effects (they never hit disk), so they need not be in the call log — the
  restored VFS already reflects them. They are recorded only if we want a
  per-write audit trail (optional; see Open Questions).

### `node:fs` surface (v1)

Sync + promise variants of: `readFile`, `writeFile`, `appendFile`, `readdir`,
`mkdir` (incl. `recursive`), `rm`/`unlink`/`rmdir`, `stat`/`lstat`, `existsSync`,
`rename`, `realpath`. `createReadStream`/`createWriteStream` are buffered shims
over the above. Everything else throws `ENOSYS`-style clear errors. Register via
the `node:` builtin allowlist — add `"fs"` to `NODE_BUILTIN_ALLOWLIST`
(`transpile.rs`) and `BUILTIN_NAMES` (`builtins.rs`) with a shim that calls into
host-bound ops (`__chidori_fs_*`) the same way `process` delegates to globals.

## Subsystem 2 — Crypto

### Surface

Install `globalThis.crypto` (Web Crypto subset) and `node:crypto`:

- `crypto.getRandomValues`, `crypto.randomUUID`, `node:crypto.randomBytes`,
  `randomFillSync`, `randomInt`, key generation → **category 2, captured +
  flagged `CryptoRandom`/`CryptoKeygen`**.
- `crypto.subtle.digest`, `createHash`, `createHmac`, `createCipheriv` over
  caller-supplied key+IV+data → **category 1, pure-in-VM, inline, flagged
  `CryptoHash`** (deterministic given inputs). No capture needed; the same
  inputs always yield the same bytes.

### Capture mechanism for randomness

Two valid strategies; we choose **(b)** as default and allow **(a)** via policy:

(a) **Seeded deterministic RNG** — extend the existing
`RandomPolicy::Seeded` PRNG (`snapshot.rs` ~line 926) to back crypto randomness
from the same `deterministic_seed` stream. Fully reproducible with *no* call-log
entries, but the output is not cryptographically strong and a replay on a fresh
seed diverges. Good for tests.

(b) **Captured real randomness** — first run draws from the host CSPRNG, the
exact bytes are written to the `CallRecord.result`, and replay returns those
bytes. Cryptographically strong *and* perfectly reproducible on resume. This is
the durable default and the reason "crypto should exist but trigger a flag the
runtime captures" — the flag is `CryptoRandom` and the captured value is the
random bytes themselves.

```
function = "crypto.randomBytes"
args     = { "n": 32 }
result   = { "bytes_b64": "…" }   // captured; replayed verbatim
```

`CryptoPolicy = Disabled | Seeded | Captured | Host`. `Captured` is durable
default; `Host` (uncaptured live randomness) is rejected by
`ensure_durable_safe` exactly like `DatePolicy::Host` / `RandomPolicy::Host`
are today.

## Subsystem 3 — Timers

Timers are the hard case because a callback can be scheduled before a snapshot
and must fire after restore. We virtualize time against a **logical clock** and
model pending timers as snapshot state.

### Logical clock

- A monotonic `logical_now_ms: u64` lives in the context, initialized from the
  `DatePolicy` base (epoch 0 under `Fixed`). `Date.now()` reads it. This unifies
  `Date` and timers under one clock — today `Date` is frozen independently; this
  work makes the frozen value *advance* as timers fire.
- Advancing the clock is deterministic: it is driven by the timer queue, not by
  wall-clock. When the microtask/macrotask queue is otherwise empty, the engine
  fast-forwards `logical_now_ms` to the earliest pending timer's deadline and
  fires it. No real sleeping occurs — a `setTimeout(fn, 5000)` resolves
  immediately in wall-clock but the in-VM clock jumps 5000ms.

### Timer queue as snapshot state

```rust
struct Timer {
    id: u64,
    due_ms: u64,         // logical deadline
    interval_ms: Option<u64>,   // Some → setInterval
    // the JS callback continuation is captured by the VM snapshot itself,
    // keyed by a stable callback handle the engine restores.
}
```

- The set of pending timers serializes into the snapshot (ordered by
  `(due_ms, id)` for deterministic fire order). On restore, the queue is rebuilt
  and execution resumes firing from the restored `logical_now_ms`.
- Each scheduled timer raises capability flag `Timer`. `queueMicrotask` raises
  `Microtask`.
- A timer that is *pending across a suspend* is represented as a
  `PendingHostOperation` of a new kind `PendingHostOperationKind::Timer`, so it
  flows through the same `HostPromiseTable` suspend/resume path that `prompt`
  and `http` already use. Its "completion" is the clock reaching `due_ms`.

### Policy

`TimerPolicy = Disabled | Virtual | Host`. `Virtual` (logical clock,
deterministic, default) and `Host` (real wall-clock; rejected for durable runs).
`Disabled` keeps today's behavior for agents that must not schedule.

### Interaction with `DatePolicy`

`DatePolicy::Fixed` becomes the *initial value* of the logical clock rather than
a permanent freeze. `DatePolicy::Disabled` still throws. A migration note:
existing snapshots created under "Date frozen at 0, no timers" remain valid
because with an empty timer queue the clock never advances — behavior is
unchanged, so `ensure_compatible` does not break old snapshots that lack a timer
section (treat absent as empty).

## Policy summary

Extend `RuntimePolicy`:

```rust
pub struct RuntimePolicy {
    pub typescript_imports: TypeScriptImportPolicy,
    pub date: DatePolicy,
    pub random: RandomPolicy,
    pub maps_sets: MapSetSnapshotPolicy,
    pub fs: FsPolicy,         // Disabled | Captured(default) | Host
    pub crypto: CryptoPolicy, // Disabled | Seeded | Captured(default) | Host
    pub timers: TimerPolicy,  // Disabled | Virtual(default) | Host
    pub deterministic_seed: String,
}
```

- `durable_default` sets `fs: Captured`, `crypto: Captured`, `timers: Virtual`.
- `ensure_durable_safe` rejects `Host` for all three (consistent with the
  existing `date=host` / `random=host` rejection).
- `ensure_compatible` gains the three fields; **older snapshots without them
  deserialize via `#[serde(default)]` to the durable defaults** so existing
  stored runs stay loadable.

## Replay & determinism rules

1. A captured effect is keyed by `seq` from the single global sequence. Replay
   matches `seq` + `function`; a mismatch aborts with the existing
   `try_replay_checked` error ("re-run without replay to regenerate").
2. Inline (pure-in-VM) effects must be **byte-deterministic** functions of VM
   state. If any doubt (e.g. a hash that incorporates a timestamp), it must be
   promoted to a captured effect.
3. The VFS, logical clock, and timer queue are **state, not effects** — they
   ride the snapshot and are never re-derived from the call log. The call log
   carries only the *boundary* values (host-seeded reads, captured randomness,
   host time samples).
4. Capability flags are derived, not authoritative for replay: recomputed on
   replay from the same operations, and asserted equal to the stored manifest
   set as a consistency check.

## node: wiring checklist

For each new builtin (`fs`, `crypto`; timers are globals, not a `node:` module
but `node:timers`/`node:timers/promises` should alias the globals):

- Add the name to `NODE_BUILTIN_ALLOWLIST` (`transpile.rs:57`) and
  `BUILTIN_NAMES` (`builtins.rs:19`) — the comment there already notes the two
  must stay in sync.
- Add a shim in `builtins.rs` that re-exports the installed globals / host ops,
  throwing a clear error on unimplemented surface (match existing
  `process`/`buffer`/`util` style).
- Host ops (`__chidori_fs_readFile`, `__chidori_crypto_randomBytes`, …) are
  registered the same way current host bindings are (`host_core.rs`), each
  going through the capture helper and `host_operation_kind` mapping.

## Testing

- **Golden replay**: run an agent that writes/reads files, draws random bytes,
  and schedules timers; snapshot mid-run; restore in a fresh process; assert
  identical final state and identical call log. (Mirror the existing
  durable-run tests in `host_core.rs`.)
- **VFS determinism**: `readdir` order, stat times, `..`-clamping, large file
  round-trip through base64 in the call log.
- **Crypto**: `Captured` randomness replays verbatim; `Seeded` reproduces from
  seed; `createHash` is inline and not call-logged; `Host` is rejected by
  `ensure_durable_safe`.
- **Timers**: logical clock fast-forward order; `setInterval` fires N times
  deterministically; a timer pending across snapshot fires correctly after
  restore; clock advances `Date.now()` consistently.
- **Capability ledger**: each surface raises exactly its expected flags; flags
  appear on the manifest and OTEL span; replay recomputes the identical set.
- **Back-compat**: a snapshot serialized before these fields existed loads with
  defaulted policy and an empty timer queue, and replays unchanged.

## Phasing

1. **Policy + capability ledger scaffold** — add the three policy fields
   (defaulted, back-compat), the `Capability` enum, `note_capabilities`, and
   manifest/OTEL emission. No new surface yet. Lands safely; everything still
   rejects.
2. **VFS** — backing store, snapshot serialization, `node:fs` shim, wire to
   `chidori.workspace`. Capability flags `Fs*`.
3. **Crypto** — `globalThis.crypto` + `node:crypto`, inline hashing, captured
   randomness, `CryptoPolicy`.
4. **Timers** — logical clock unification with `DatePolicy`, virtual timer
   queue, snapshot-resident pending timers, `PendingHostOperationKind::Timer`.
5. **Hardening** — approval gating on capability flags, fuzz the VFS path
   normalizer, parity tests against Node for the covered surface subset.

## Open questions

- **Per-write audit**: do we call-log VFS writes for a forensic trail, or rely
  solely on snapshot state? (Leaning: snapshot only, optional debug-mode audit.)
- **VFS seed size**: bytes inline in the snapshot vs. content-addressed external
  blobs with hashes in the snapshot. Large seeds argue for the latter.
- **`crypto.subtle` async**: it returns Promises even for pure digests; confirm
  the inline path can resolve synchronously within the VM's job queue without a
  host round-trip.
- **Interval cancellation across snapshot**: a `setInterval` cleared in code
  that ran pre-snapshot vs. an id restored post-snapshot — confirm `clearInterval`
  by id is stable across the serialize boundary.
- **Capability-based blocking**: should any flag (e.g. `CryptoKeygen`) default
  to a `PolicyApproval` pause, or is visibility-only the v1 contract? (Leaning:
  visibility-only; blocking is opt-in policy.)
