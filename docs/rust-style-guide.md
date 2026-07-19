---
title: "Rust Style Guide"
---

# Rust style guide

How we write Rust in this repository. The goal is a codebase that reads as if
one careful person wrote it: predictable structure, errors that carry context,
comments that explain *why*, and a formatter/linter setup that makes style a
non-topic in review.

This guide is grounded in the [Rust API Guidelines], the official
[Rust Style Guide] (what `rustfmt` implements), the [Clippy lint
groups][clippy-lints], and the panic policy articulated in
["Using unwrap() in Rust is Okay"][burntsushi-unwrap] — adapted to this
workspace's specifics. Where existing code diverges from a rule, the rule wins
for new code; see [Adopting this guide](#adopting-this-guide) for how to handle
the gap.

[Rust API Guidelines]: https://rust-lang.github.io/api-guidelines/
[Rust Style Guide]: https://doc.rust-lang.org/style-guide/
[clippy-lints]: https://doc.rust-lang.org/clippy/lints.html
[burntsushi-unwrap]: https://burntsushi.net/unwrap/

## Scope

Applies to all Rust in the workspace:

| Crate | Role | Special rules |
|---|---|---|
| `crates/chidori` | Agent framework + CLI binary | `anyhow` errors, `tokio` async, `tracing` |
| `crates/chidori-js` | Pure-Rust JS engine (lib) | Synchronous, `Result<_, Value>` errors, **zero `unsafe`** |
| `crates/test262-runner` | Conformance harness (`publish = false`) | Test-tool leniency: `unwrap`/`expect` fine |

## Toolchain and formatting

- **Toolchain** is pinned by `rust-toolchain.toml` (stable channel, `rustfmt` +
  `clippy` components). The workspace MSRV is `rust-version = "1.95"` in each
  crate's `Cargo.toml`; bump it deliberately and note why (the current floor
  comes from `oxc`).
- **Formatting is default `cargo fmt`, no configuration.** There is
  intentionally no `rustfmt.toml`: the upstream default style is the house
  style, and every toolchain update applies it identically for everyone.
  Never hand-format against the formatter. The pre-commit hook (`hk.pkl`,
  installed via `mise`) auto-formats staged files, and CI rejects unformatted
  code with `cargo fmt --check`.
- `#[rustfmt::skip]` is reserved for genuinely tabular code — opcode tables,
  test matrices, aligned bytecode listings — where alignment carries meaning.
  Add a one-line comment saying what the alignment communicates.
- **Edition 2021** across the workspace. Edition bumps are a workspace-wide,
  single-PR affair.

## Clippy

Clippy is enforced: CI and the pre-commit hook both run

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

so run it locally before pushing. Guidance on lint levels:

- The default groups (`correctness`, `suspicious`, `complexity`, `perf`,
  `style`) are the baseline. `correctness` findings are bugs — fix them, never
  suppress them.
- The few workspace-wide exceptions live in `[workspace.lints.clippy]` in the
  root `Cargo.toml` (each crate inherits via `[lints] workspace = true`), each
  with a comment saying why. That table is the *only* place for blanket
  allows; think hard before growing it.
- When a lint is wrong for a specific site, suppress it **narrowly** (item
  level, not module level) and prefer `#[expect(clippy::..., reason = "...")]`
  over `#[allow]`: `expect` warns when the suppression goes stale, and the
  mandatory reason documents the judgment call. Existing module-wide allows
  (`#![allow(clippy::all)]` on generated `unicode_tables.rs`,
  `#![allow(dead_code)]` on not-yet-wired runtime modules) are the pattern for
  the *rare* legitimate blanket case: generated code and scaffolding, with the
  module header saying so.
- Some discipline goes beyond what the default groups machine-check; treat
  these as always-on reviewer expectations: `undocumented_unsafe_blocks`
  (every `unsafe` block gets a `// SAFETY:` comment), `await_holding_lock`
  (no sync lock guards across `.await`), `dbg_macro` and stray
  `todo!`/`unimplemented!` (never merged), `let_underscore_future` (a dropped
  future silently does nothing).

## Naming

Follow [RFC 430 casing][c-case] and the API-guidelines conventions; the ones
that come up most here:

- Types, traits, enum variants: `UpperCamelCase`. Functions, methods, modules,
  fields: `snake_case`. Constants and statics: `SCREAMING_SNAKE_CASE`. Crate
  features: concrete words, no `use-` / `with-` placeholders.
- Conversions follow [`as_` / `to_` / `into_`][c-conv]: `as_` is a free
  borrowed→borrowed view, `to_` is an expensive copy, `into_` consumes `self`.
- Getters are the bare field name (`fn seq(&self) -> Seq`), not `get_seq`;
  `get` is reserved for lookups that take a key and may miss.
- Iterator-producing methods are `iter()`, `iter_mut()`, `into_iter()`
  ([C-ITER][c-iter]); a custom iterator type is named after the method that
  produces it.
- Name things for the domain, and keep word order consistent with the
  neighbors: an error variant family that starts as `ParseError`,
  `CompileError` should not grow an `ErrorLimit`.

[c-case]: https://rust-lang.github.io/api-guidelines/naming.html#c-case
[c-conv]: https://rust-lang.github.io/api-guidelines/naming.html#c-conv
[c-iter]: https://rust-lang.github.io/api-guidelines/naming.html#c-iter

## Modules and visibility

- **Directory modules use `mod.rs`** (`runtime/mod.rs`, `providers/mod.rs`,
  ...). That is the established layout; don't mix in the `foo.rs` +
  `foo/` sibling style — pick up the convention that is already there.
- `chidori-js` stays **flat**: one file per engine concern (`vm.rs`, `gc.rs`,
  `exec.rs`). Split a file when it grows a second responsibility, not at a
  line count.
- Every module starts with a `//!` header: what the module is, the one or two
  invariants a reader must know, and pointers to the design doc under `docs/`
  when one exists. Most of the tree already does this; new modules must.
- **Default to `pub(crate)`.** Reach for plain `pub` only when the item is
  deliberately part of the crate's public API — for `chidori`, that means it's
  re-exported through the curated `framework` facade or otherwise documented
  as stable. A `pub` item is a compatibility promise; a `pub(crate)` item is
  refactorable on a whim. (The current tree is `pub`-heavy; tighten
  opportunistically when touching a module.)
- Keep `lib.rs` a table of contents: module declarations, curated re-exports,
  crate docs. Logic lives in modules.

## Error handling

The workspace deliberately runs **two error regimes**. Know which side of the
boundary you're on, and don't let them leak into each other.

### `chidori` (framework, CLI, server): `anyhow`

Application-style code uses `anyhow::Result<T>` end to end:

- Propagate with `?`. Attach context at the point where a failure would
  otherwise be ambiguous:

  ```rust
  let manifest = fs::read_to_string(&path)
      .with_context(|| format!("reading package manifest {}", path.display()))?;
  ```

  Use `.context(...)` for cheap static messages and `.with_context(|| ...)`
  when the message allocates. A good context line names the *operation and the
  subject* ("reading package manifest X"), not the error class ("IO error").
- `bail!` and `ensure!` for early exits and precondition checks that are
  *error conditions* (bad user input, malformed config) rather than bugs.
- Don't introduce `thiserror` enums or custom error types unless a caller
  genuinely needs to **match** on the failure (e.g. a provider distinguishing
  retryable rate limits from fatal auth errors). One unified error chain that
  prints well is the point of the anyhow regime. If you do add a typed error,
  it implements `std::error::Error` + `Debug` + `Display` and converts into
  `anyhow::Error` at the boundary for free.

### `chidori-js` (engine): errors are JS values

Inside the engine, a failure is a **thrown JavaScript exception**, so the
idiom is `Result<T, Value>` where the `Err` payload is the JS value routed
through `Completion::Throw`. Constructor selection goes through the engine's
`ErrorKind` mapping.

- Never import `anyhow` (or any other error crate) into `chidori-js`. If an
  engine-internal failure can't be expressed as a throwable JS value, it is a
  bug — see panics below.
- Host-boundary code in `chidori` that calls into the engine converts at the
  boundary: JS exception → structured error/context on the anyhow chain, once,
  in one place.

### Panics: bugs, not errors

We follow the [bug vs. error condition distinction][burntsushi-unwrap]: a
`Result` models something that is *expected to fail* sometimes (I/O, user
input, network, subprocesses); a panic asserts an *invariant* — something that
cannot happen unless the program has a bug.

- **In `Result`-returning code, reach for `?`, not `.unwrap()`.** An
  `unwrap()` on an operation that can legitimately fail converts a recoverable
  error into a process abort — in a runtime that promises durable, resumable
  agents, that's the worst possible failure mode.
- `unwrap()`/`expect()` are acceptable when the failure would be a bug:
  a mutex poisoned only if another thread already panicked, a regex literal
  known valid at compile time, an index established by the loop above, a
  key inserted two lines earlier. Prefer `expect("...")` when the message adds
  diagnostic value a stack trace wouldn't; write it as the *invariant that was
  supposed to hold* ("bytecode register allocated by compile pass"), not a
  restatement of the failure ("failed to get register").
- `panic!`, `assert!`, `debug_assert!` follow the same rule: invariants only.
  Prefer `debug_assert!` for hot-path checks the release interpreter can't
  afford.
- **Tests, benches, examples, and `test262-runner` may unwrap freely.** A
  panic is exactly the right test-failure mechanism, and `?`-plumbing in tests
  obscures the assertion under test.
- Never `unwrap()` on a value derived from user input, agent code, the
  network, the filesystem, or an LLM response. Agents run arbitrary
  TypeScript; anything they can influence is an error condition by
  definition.

## Logging and output

- **Library and runtime code speaks `tracing`, never `println!`.** Everything
  under `runtime/`, `providers/`, `mcp/`, `pkg/` (except interactive install
  progress), `server.rs`, `scheduler.rs`, `storage.rs`, and all of
  `chidori-js` uses `tracing::{trace,debug,info,warn,error}` with structured
  fields:

  ```rust
  tracing::debug!(session_id = %id, seq, "replaying journal entry");
  ```

  Structured fields (`field = value`), not format-string interpolation — the
  OTLP exporter and any downstream collector can only filter on fields.
- `println!`/`eprintln!` are for **CLI user-facing output only**: `main.rs`
  command handlers, `init.rs`, interactive prompts, install progress. Rule of
  thumb: if the text is the *product* of the command, print it (results to
  stdout, user-facing errors to stderr); if it describes what the program is
  doing internally, it's a `tracing` event.
- Pick levels for an operator reading production logs: `error` = something
  was lost or a request failed; `warn` = degraded but recovered; `info` =
  lifecycle milestones (server started, session resumed); `debug`/`trace` =
  developers only. Don't log at `info` inside per-opcode or per-journal-entry
  loops.
- No `dbg!` in committed code.

## Async and concurrency

`chidori` is async on `tokio`; `chidori-js` is deliberately **synchronous**
(determinism and replay depend on it) and must stay free of async runtimes.

- **Never block a runtime worker thread.** CPU-heavy or blocking work
  (synchronous file I/O bursts, subprocess waits, running the JS engine) goes
  through `tokio::task::spawn_blocking` or a dedicated thread. JS execution
  threads must be spawned with the `JS_THREAD_STACK_BYTES` stack size — the
  interpreter is deeply recursive and the default stack will overflow.
- **No lock guards across `.await`.** Use `std::sync::Mutex` for short
  critical sections that never span an await; if you genuinely must hold a
  lock across an await point, that's the one case for `tokio::sync::Mutex` —
  but first ask whether a channel or an owning task removes the shared state
  entirely. Message passing over shared locks where either fits.
- `tokio::spawn` is for actual concurrency, not a cheap function call. A
  spawned task's `JoinHandle` is either awaited or its abandonment is
  explicit and commented; `let _ = some_future;` silently does nothing.
- Be deliberate about **cancellation safety** in `select!` loops: a branch
  losing the race drops its future mid-flight. Anything with side effects that
  must complete (journal writes, checkpoint flushes) belongs in a task or a
  cancel-safe pattern, not directly in a `select!` arm. Use RAII/drop guards
  for cleanup that must run even when a task is cancelled.
- Bound everything: channels get explicit capacities, retries get caps and
  backoff, per-run resource use goes through the existing limit machinery.

## Unsafe code

- **`chidori-js` contains zero `unsafe` and must stay that way** — "pure-Rust,
  no `unsafe`" is a documented property of the engine (see
  `docs/architecture.md`) and part of the sandbox story.
- In `chidori`, `unsafe` is confined to the OS-isolation layer
  (`runtime/isolate/*` — rlimits, seccomp, landlock FFI) and `mem_guard.rs`.
  New `unsafe` outside that neighborhood needs a strong justification in the
  PR description.
- Every `unsafe` block is minimal (one operation per block where practical)
  and carries a `// SAFETY:` comment stating the invariant that makes it
  sound — what must be true, and why it is true *here*:

  ```rust
  // SAFETY: setrlimit is async-signal-safe and `lim` is a valid, initialized
  // rlimit struct; we are single-threaded post-fork at this point.
  unsafe { libc::setrlimit(libc::RLIMIT_AS, &lim) };
  ```

## Documentation and comments

The house comment style is **prose-heavy and rationale-driven** — see the
workspace `Cargo.toml` or any `runtime/` module header for the register. Keep
it that way:

- `//!` module docs on every module; `///` docs on every `pub` item. For
  fallible or panicking public functions, document the failure modes
  (`# Errors`, `# Panics`) — [C-FAILURE].
- Comments explain **why, not what**: the constraint, the invariant, the
  rejected alternative, the workaround's upstream issue link. If a comment
  paraphrases the next line of code, delete it. If a decision took you ten
  minutes of thought, spend one more writing down the reasoning — the comment
  is for the person (possibly you) who would otherwise have to re-derive it.
- Design-level reasoning goes in `docs/*.md` and gets linked from the module
  header, not duplicated across code comments. New subsystems of any size get
  a design doc; that's the established pattern (~30 docs and counting).
- Doc examples compile (they're tests). Use `?` in examples rather than
  `unwrap()` ([C-QUESTION-MARK]).

[C-FAILURE]: https://rust-lang.github.io/api-guidelines/documentation.html#c-failure
[C-QUESTION-MARK]: https://rust-lang.github.io/api-guidelines/documentation.html#c-question-mark

## Testing and benchmarks

- **Unit tests** live inline in `#[cfg(test)] mod tests` next to the code they
  exercise. **Integration tests** live in the crate's `tests/` directory and
  exercise the public surface. Async tests use `#[tokio::test]`.
- Engine language behavior is validated by **Test262** via `test262-runner`
  (see `docs/conformance.md`) — don't hand-write unit tests for spec behavior
  the suite already covers; do add a focused regression test when fixing a bug
  the suite missed.
- Name tests for the behavior and the condition
  (`resume_skips_replayed_llm_calls`), not `test_1`. A failing test's name
  should tell you what broke before you open the file.
- Tests must be deterministic and parallel-safe: `tempfile` for filesystem
  state, no fixed ports, no sleeps as synchronization, no reliance on wall
  clock. A test that needs the network is an integration test behind an
  explicit opt-in, never part of `cargo test --workspace`.
- Performance work goes through the **criterion** benches (`benches/`) and the
  JS workload suite; claims about speedups in PRs come with bench numbers.
  `cargo test --workspace` plus `cargo fmt --check` is the CI gate — keep both
  green locally before pushing.

## Dependencies and Cargo

- Adding a dependency is an architectural decision: prefer std and existing
  deps first; check maintenance, transitive weight, and license
  (workspace is Apache-2.0; deps must be permissively licensed). Say why in
  the PR.
- **No unused dependencies.** A declared-but-unused crate is worse than a
  missing one — it implies a convention nobody follows.
- TLS is `rustls` everywhere (`reqwest` is configured without OpenSSL); don't
  introduce anything that links OpenSSL or other C libraries lightly — the
  single-static-binary install story depends on it.
- Platform-specific code is `cfg`-gated the way `runtime/isolate` already is
  (`cfg(unix)`, `cfg(target_os = "linux")`), with a portable fallback or a
  clear compile error, and gets covered by the `os-isolation` CI workflow when
  it touches sandboxing.
- Shared package metadata is inherited from `[workspace.package]`
  (`field.workspace = true`); keep new common fields there rather than
  repeating them per crate.

## Public API design

For the publishable crates (`chidori`, `chidori-js`), the [Rust API
Guidelines] checklist applies; the highlights we care most about:

- Derive the common traits eagerly where semantically valid: `Debug` on every
  public type (non-negotiable — [C-DEBUG]), plus `Clone`, `PartialEq`/`Eq`,
  `Hash`, `Default`, and `serde` derives where they make sense.
- Prefer **newtypes over primitives** for identifiers and quantities that can
  be confused (`Seq`, session ids, byte budgets) — a `u64` parameter named
  `seq` next to one named `limit` is an accident waiting to happen
  ([C-NEWTYPE]).
- Avoid boolean and `Option` parameters that read as mystery values at the
  call site; use a two-variant enum or a builder ([C-CUSTOM-TYPE],
  [C-BUILDER]). Constructors are static inherent methods (`new`,
  `with_config`), conversions use `From`/`TryFrom` rather than ad-hoc methods.
- Keep struct fields private and take `impl AsRef<Path>` / `impl Into<String>`
  style generics at ergonomic boundaries — but only where the flexibility is
  actually used; don't generalize speculatively.

[C-DEBUG]: https://rust-lang.github.io/api-guidelines/debuggability.html#c-debug
[C-NEWTYPE]: https://rust-lang.github.io/api-guidelines/type-safety.html#c-newtype
[C-CUSTOM-TYPE]: https://rust-lang.github.io/api-guidelines/type-safety.html#c-custom-type
[C-BUILDER]: https://rust-lang.github.io/api-guidelines/type-safety.html#c-builder

## Adopting this guide

Parts of the tree predate these rules — most visibly the density of
`.unwrap()` in `crates/chidori/src` and stray `println!` in runtime modules.
The policy for closing the gap:

1. **New and modified code follows the guide.** Review against it.
2. **Clean up opportunistically, in scope.** When a change touches a function,
   bring that function up to the guide (convert unwraps on fallible
   operations to `?` + context, replace runtime `println!` with `tracing`).
3. **No drive-by churn.** Don't send mass mechanical rewrites mixed into
   feature work; a focused, reviewable cleanup PR for one module at a time is
   fine.
4. Clippy is already gated in CI and the pre-commit hook, so the tree stays
   warning-free by construction; the remaining gap is the `unwrap()` /
   `println!` backlog, which items 1–3 burn down over time.

## References

- [Rust API Guidelines] and the [checklist](https://rust-lang.github.io/api-guidelines/checklist.html)
- [Rust Style Guide] (the spec `rustfmt` implements)
- [Clippy lint list][clippy-lints] and [usage](https://doc.rust-lang.org/clippy/usage.html)
- ["Using unwrap() in Rust is Okay"][burntsushi-unwrap] — the panic policy this guide adopts
- [Tokio tutorial](https://tokio.rs/tokio/tutorial) — spawning, shared state, channels
- [`tokio::sync::Mutex` docs](https://docs.rs/tokio/latest/tokio/sync/struct.Mutex.html) — on when *not* to use it
- Repo-specific design docs: [`docs/architecture.md`](./architecture.md),
  [`docs/sandbox-model.md`](./sandbox-model.md),
  [`docs/replay.md`](./replay.md), [`docs/conformance.md`](./conformance.md)
