---
title: "Conformance (Test262)"
description: "Test262 methodology and the CI conformance gate for the pure-Rust JS engine."
---

# JavaScript conformance: running chidori against Test262

Chidori executes agent code on its **pure-Rust JavaScript engine**
(`crates/chidori-js`, oxc parser, zero `unsafe`) — the only JS engine in
the tree. To answer "is our JavaScript runtime at parity with Bun and Node?" we
run it against **Test262**, the official TC39 ECMAScript conformance suite and
the one corpus that both Bun (JavaScriptCore) and Node (V8) publish
language-conformance numbers against. Test262 is therefore the apples-to-apples
yardstick; runtime-specific suites (`Bun.serve`, `node:test` internals, etc.)
test product surface that does not generalize.

Test262 measures the *language*. For the **Node builtin shims** (`node:fs`,
`node:stream`, …) the equivalent yardstick is Node core's own test suite — a
curated subset is vendored and run by the node-compat harness
(`crates/chidori/src/node_compat.rs`), with results tracked in
[`docs/node-compat-report.md`](node-compat-report.md) and gated in CI by
`crates/chidori/tests/node_compat/expectations.json` (same
baseline-with-intentional-updates model as the Test262 gate below).

Because `chidori-js` has no fallback engine, conformance is load-bearing: a
language regression directly breaks real agents. CI gates every
engine change against a committed baseline (see [CI gate](#ci-gate)).

## TL;DR

```bash
# Vendor the pinned suite and run language + built-ins:
scripts/test262.sh

# Run a subset:
scripts/test262.sh test/built-ins/Array
scripts/test262.sh --filter Promise

# Gate against the committed baseline (non-zero exit on a regression):
scripts/test262.sh --gate

# Re-record the baseline after an intentional conformance change:
scripts/test262.sh --update-baseline
```

The runner prints, e.g.:

```
Test262 (chidori pure-Rust engine, bare context)
  pass 40758  fail 4  skip 6529  =>  99.99% of executed
```

## Current result

Against `test/language` + `test/built-ins` (scripts **and** modules), at the
pinned suite commit:

| | pass | fail | skip | % of executed |
|---|---|---|---|---|
| chidori pure-Rust engine, bare context | 40,758 | 4 | 6,529 | **99.99%** |

The headline percentage is `pass / (pass + fail)` over *executed* tests; the
skip count is reported alongside so the denominator is never hidden.

Every executed directory except `language/literals/regexp` passes 100%:
all of `built-ins/*` (RegExp, Promise, Function, Array, TypedArray, Proxy,
the whole iterator/async-iterator surface, …) and all of `language/*`
including the once-hard clusters — module namespaces and their TDZ
internals, top-level-await evaluation ordering (spec
`[[AsyncEvaluationOrder]]`/`[[CycleRoot]]` semantics), dynamic `import()`
(errored-module caching, in-flight-module waiting), `import.meta`,
global/eval lexical bindings, `with`-statement bindings, mapped arguments,
and destructuring/relational evaluation order. Recently promoted out of the
skip list (implemented and now held to account): iterator helpers,
`Array.fromAsync`, `Uint8Array` base64/hex, and duplicate named capture
groups.

## What is measured, and why "bare context"

The runner drives the **bare ECMAScript context** — a fresh `chidori-js` VM with
*no* `chidori` host object and *no* captured-effect prelude installed. That
isolates pure language conformance, exactly how Bun and Node report their
Test262 numbers. Chidori's differentiators (security sandboxing, deterministic
captured effects, replay/snapshot) are layered *on top of* this context;
measuring the bare context first tells us whether the language substrate is
sound before the durability layer is added.

For each test file the runner:

- parses the `/*--- ... ---*/` YAML frontmatter (`flags`, `includes`,
  `negative`, `features`);
- selects execution variants per the `flags` rules — `raw`; `module`; otherwise
  both `sloppy` and `strict` (honoring `onlyStrict` / `noStrict`);
- spins up a **fresh VM per variant** for isolation, installs a `print`/`$262`
  bootstrap plus the harness includes (`assert.js`, `sta.js`,
  `doneprintHandle.js`, and any `includes:`);
- runs the body, handling positive tests (must not throw; `async` tests must
  signal completion after the job queue drains), negative-parse tests
  (compile-only; must throw the named error), and negative runtime/resolution
  tests (run, drain jobs, must throw the named error);
- reports `pass` / `fail` / `skip` per file.

`module`-flag tests **run by default** (the runner resolves their fixture
imports); pass `--no-modules` to skip them.

Dynamic `import()` also runs: the runner installs the engine's
`Vm::dynamic_import` host hook, resolving specifiers against the test file's
directory and sharing one module registry per test (so a specifier reached
both statically and dynamically yields the same namespace object). Without a
hook installed — e.g. in the production chidori runtime, which forbids dynamic
import by policy — `import()` rejects with a TypeError.

## Parallel execution

The runner fans the per-file loop out across **one worker per CPU** by default
(override with `TEST262_JOBS`). Workers pull file indices off a shared atomic
cursor — dynamic load-balancing, because second-level directories vary by orders
of magnitude in cost — and each holds its own harness cache. Per-test
timeout/panic isolation holds regardless: every execution runs on its own
worker thread, which confines the `Rc`-based (non-`Send`) engine to that thread.

Results are **merged back in path order**, so the printed totals, the `--json`
report, the `--state` store, and the `--baseline` gate are byte-for-byte
identical regardless of how many workers ran or how the work was scheduled
(`TEST262_JOBS=1` and `TEST262_JOBS=16` produce the same output). On a 4-core box
the file loop runs ~3–4× faster; it scales with core count.

Each test also has a wall-clock budget (`TEST262_TIMEOUT_MS`, default 5 s) so a
single pathological test — a catastrophic regex, a near-op-budget loop — is
recorded as a timeout failure instead of stalling a worker. A conformant engine
runs each Test262 file in well under a second, so the budget only ever catches
pathologies. The gate scripts pin this explicitly (see [CI gate](#ci-gate)) so
the committed baseline is reproducible no matter what the compiled-in default is.

## Why the run is chunked

`chidori-js` uses reference-counting (`Rc<RefCell<…>>`); cycles are reclaimed
by the engine's cycle collector (`crates/chidori-js/src/gc.rs`): every
allocation is registered per-VM, `Vm::dispose()` breaks the outgoing edges of
**every** object the VM ever allocated (including orphaned cycles disconnected
from the realm roots), and `Vm::collect_cycles()` offers mark-sweep for
long-lived VMs. Since the runner disposes a fresh VM per test, memory across a
single-process run stays flat (~20 MB RSS over the 21k `language/` tests).

Both `scripts/test262.sh --gate` and `--update-baseline` run the suite
**one second-level directory at a time, in a fresh process each**, for crash
isolation: a single engine abort (e.g. a stack overflow on a pathological
test) kills only its chunk, not the whole sweep.
The runner's `--state <file>` flag merges per-test results across chunks;
`--baseline <file>` gates each chunk against the full baseline. Each chunk
runs its own files in parallel across all cores (see [Parallel
execution](#parallel-execution)), so a full chunked pass scales with the core
count of the box (or CI runner) it lands on.

## Honest skips

The runner **skips** (does not count as failure) tests that require features the
engine intentionally does not implement — the same way Bun/Node skip what their
engines lack. The list lives in `UNSUPPORTED_FEATURES` in
`crates/test262-runner/src/main.rs` (e.g. `decorators`,
`import-attributes`, `WeakRef`/`FinalizationRegistry`), plus `intl402/`
(skipped unless `--intl`), Temporal-tagged tests (skipped unless
`--temporal`), and the agent (`CanBlock`, and the
`atomicsHelper.js` multi-agent harness) tests. A handful of tests call
`$262.createRealm` without carrying the `cross-realm` feature tag; the
runner detects the call in source and gives them the same honest skip as
the tagged tests (a second realm cannot be hosted either way). When the engine grows to cover a
skipped feature, delete its entry and the suite starts holding it to account.

`SharedArrayBuffer` and `Atomics` **are** implemented (their feature tags are
not skipped). The embedded runtime is single-threaded — the engine is
`Rc`-based and non-`Send` — so a SharedArrayBuffer is an ArrayBuffer that never
detaches and grows in place, and every Atomics operation is a sequential
read / read-modify-write, observationally identical to a real atomic on a single
agent. `Atomics.wait` reports that the calling agent cannot block (as a browser
main thread does); `Atomics.waitAsync` and the genuinely-concurrent
`$262.agent` tests stay skipped, since a second agent cannot be hosted.

## Intl (opt-in: `--intl`)

A foundational slice of ECMA-402 is implemented, backed by ICU4X
(`icu_locale_core` + the CLDR-data `icu_locale` canonicalizer/expander, plus
`icu_plurals` + `fixed_decimal`):

- the `Intl` namespace and `Intl.getCanonicalLocales`;
- the full `Intl.Locale` constructor + prototype
  (`baseName`/`language`/`script`/`region`/`variants` and the
  `calendar`/`collation`/`hourCycle`/`caseFirst`/`numeric`/`numberingSystem`
  Unicode-extension accessors, plus `maximize`/`minimize`/`toString`);
- `Intl.PluralRules` (`select`, `selectRange`, `resolvedOptions`,
  `supportedLocalesOf`), with cardinal/ordinal rules and the
  fraction/significant digit operand options;
- `Intl.NumberFormat` (`format`, `formatToParts`, `resolvedOptions`,
  `supportedLocalesOf`) for the `decimal` and `percent` styles — full option
  parsing/validation, locale-aware grouping and numbering systems (via
  `icu_decimal`), the integer/fraction/significant digit options, all nine
  rounding modes, and `signDisplay`. It is callable with or without `new`,
  and `format` is the spec's once-bound getter.

Against `test/intl402/Intl` + `Locale` + `PluralRules` + `NumberFormat` (run
with `--intl`) the engine passes **317** of the executed tests.

Not implemented (so failing/skipped under `--intl`): the other
formatters (`DateTimeFormat`, `Collator`, `ListFormat`, …),
`Intl.supportedValuesOf`, the `Intl.Locale-info` accessors
(`getCalendars`/`getWeekInfo`/…, an honest skip via that feature tag), and —
for `NumberFormat` — the `currency`/`unit` styles, `compact`/`scientific`/
`engineering` notation, `formatRange`/`formatRangeToParts`, and
`roundingIncrement` (all of which need ICU4X's experimental formatters or the
increment-decomposition table). Also missing: full best-fit/lookup locale
resolution (`supportedLocalesOf` over-returns), `PluralRules` compact-notation
operands and `selectRange`'s CLDR plural-range table (only in ICU4X's
`unstable` surface; approximated by the end value's category), and the long
tail of Unicode-extension *keyword-value* canonicalization (e.g.
`-u-ca-gregorian` → `-u-ca-gregory`), which needs the CLDR bcp47 alias tables
the `icu_locale` canonicalizer does not apply. `intl402/` is skipped in
the default gate (it is opt-in via `--intl`), so this surface is not part
of the committed baseline.

## Temporal (opt-in: `--temporal`)

The TC39 Temporal proposal is implemented on top of
[`temporal_rs`](https://crates.io/crates/temporal_rs) (the proposal's Rust
reference implementation: ISO-calendar arithmetic, durations, rounding, time
zones). Each `Temporal.*` instance stores its backing `temporal_rs` value in an
`Internal::Temporal` slot (a GC leaf — no JS references).

All eight Temporal types are implemented, plus `Temporal.Now`. Against the
full `test/built-ins/Temporal` tree (run with `--temporal`) the engine passes
**3,886** of 4,603 executed tests (**84.4%**). Per type:

| type | pass / executed |
|---|---|
| `Duration` | 452 / 540 |
| `PlainTime` | ~468 / 493 |
| `PlainDate` | 521 / 652 |
| `Instant` | 412 / 465 |
| `PlainDateTime` | 652 / 773 |
| `PlainYearMonth` | 484 / 509 |
| `PlainMonthDay` | 184 / 199 |
| `ZonedDateTime` | 600 / 901 |
| `Now` | 52 / 66 |

Each type covers its constructor, accessors, arithmetic (`add`/`subtract`/
`until`/`since`/`round`/`total` as applicable), `with`/`withCalendar`/
`withTimeZone`, `equals`/`compare`, the cross-type converters
(`toPlainDate`/`toPlainDateTime`/`toInstant`/…), `toString`/`toJSON`/
`toLocaleString` with their rounding/calendar/offset display options, and
`from`. `Duration.round`/`total`/`compare` honor a PlainDate `relativeTo`.
`Temporal.Now` reads the system clock — the one real-clock surface in the
bare conformance context (`Date` never reads the host clock there by engine
policy: it ticks a deterministic monotonic counter, 1ms per read); the
durable runtime captures time as an effect at a higher layer.

The residual failures are concentrated in `ZonedDateTime`'s full property-bag
`with` (a `PartialZonedDateTime` not yet wired), ZonedDateTime `relativeTo`,
some non-ISO calendar corners, and option-read-order details. Temporal-tagged
tests are skipped in the default gate (opt-in via `--temporal`), so this
surface is not part of the committed baseline.

## CI gate

`.github/workflows/test262.yml` runs `scripts/test262.sh --gate` on:

- pull requests that touch the engine, the runner, the script, or the workflow;
- pushes to `main` touching those paths;
- a nightly schedule (so the number can't rot silently even when the engine is
  untouched); and
- manual `workflow_dispatch`.

The gate compares the current run against the committed baseline
(`crates/test262-runner/test262-expectations.json`, ~4 MB, one line per test) and
**fails only on a regression** — a test the baseline records as `pass` that now
fails, or a failing test absent from the baseline. Newly *passing* tests never
break the build; they print a
hint to refresh the baseline. After an intentional conformance change, run
`scripts/test262.sh --update-baseline` and commit the diff (each flipped test is
a single readable line in review).

## Remaining gaps

The committed baseline records **4** residual failures, all in one cluster
(the table and the headline number come from the same
`test262-expectations.json`):

| area | nature |
|---|---|
| `language/literals/regexp` (`S7.8.5_A1.1_T2`, `A1.4_T2`, `A2.1_T2`, `A2.4_T2`) | `eval("/" + String.fromCharCode(cu) + "/").source` must round-trip every code unit, LONE SURROGATES included. The engine's source pipeline is UTF-8 (`&str` into the oxc parser), so an unpaired surrogate in eval'd source text becomes U+FFFD before the parser ever sees it. Fixing this needs a WTF-8 source path through the front end — an architectural change, deliberately deferred. String VALUES are unaffected (`JsString` is WTF-8 and round-trips surrogates); only surrogates in *source text fed to `eval`* are lossy. |

Each failure is individually identifiable from a `--json` report.

## What the deviations mean for determinism and replay

The question a durability adopter actually asks is not "how many Test262
failures" but "can a deviation desynchronize a recorded run". Classified
against the committed baseline's 4 failures:

- **Every remaining deviation is *stable*: the engine produces the same
  (spec-divergent) result on every execution of the same program.** There is
  no randomness or environment dependence in the failing cluster. Within one
  engine build, record and replay therefore see byte-identical behavior —
  a spec deviation cannot, by itself, perturb a journal. This is not an
  assertion but a **CI-enforced invariant**: `scripts/test262.sh --stability`
  re-runs every baseline-failing test three times (`test262-runner
  --repeat 3`) and fails on any outcome that varies between runs; the
  `stability` job in `.github/workflows/test262.yml` runs it beside the
  sharded gate. (The gated surface also carries no ambient nondeterminism to
  leak into a result — `Date` reads a deterministic monotonic counter that
  advances 1ms per read (never the host clock, so the same program sees the
  same readings on every run while elapsed-time polling still terminates)
  and `Math.random()` is seeded per VM, by engine policy; the one real-clock
  surface, `Temporal.Now`, is opt-in and outside the committed baseline.)
- **The one failing cluster is a value-shape deviation**: a lone surrogate
  in `eval`'d *source text* reaches the parser as U+FFFD (see [Remaining
  gaps](#remaining-gaps)), producing a wrong-but-fixed `source` string. The
  record/replay argument above applies unchanged; the exposure is
  **cross-build replay** — a journal recorded on a build with the deviation,
  replayed on a build that fixed it, diverges and fails **loudly** at the
  first divergent call (`try_replay_checked` compares function + arguments),
  never silently.

Separately from Test262: the engine's *optimization tiers* are the one place
an execution-order perturbation could differ between two executions of the
same build. That surface is held to byte-identical observable behavior by
differential test suites that run the same programs with each pass toggled
(`tests/fusion.rs`, `tests/localize.rs`, `tests/kernels.rs`, register vs
stack tier), and replay's positional divergence checks are the backstop.

## Reproducibility

`scripts/test262.sh` vendors `tc39/test262` pinned to a specific commit
(`TEST262_REF` in the script) so the number is reproducible; bump it
deliberately — and refresh the baseline — when tracking newer language
proposals. Set `TEST262_DIR` to point at an existing checkout. The vendored
suite (`vendor/test262/`) is git-ignored.

## CLI reference

```
test262-runner [--test262 <dir>] [--filter <substr>] [--max <n>]
               [--json <out>] [--state <file>] [--baseline <file>]
               [--verbose] [--no-modules] [--intl] [--temporal] [paths...]
```

- `--test262 <dir>` — Test262 root (else `$TEST262_DIR`, else `vendor/test262`).
- `paths...` — files/dirs relative to the root (default `test/language` and
  `test/built-ins`).
- `--filter <substr>` — only run paths containing the substring.
- `--max <n>` — stop after `n` files (smoke runs).
- `--json <out>` — write a per-file JSON report.
- `--state <file>` — persist/merge per-test results across runs (used to
  accumulate chunked results).
- `--baseline <file>` — gate against committed expectations; exit non-zero only
  on a regression.
- `--verbose` — print each failure with the thrown message.
- `--no-modules` — skip `module`-flag tests (they run by default).
- `--intl` — opt into `intl402` tests.
- `--temporal` — opt into `Temporal`-tagged tests.

Environment:

- `TEST262_JOBS` — parallel workers (default: one per CPU). `1` forces a serial
  run; results are identical either way.
- `TEST262_TIMEOUT_MS` — per-test wall-clock budget (default 5000). The gate
  scripts pin this so the committed baseline stays reproducible.
- `TEST262_DIR` — an existing Test262 checkout to use instead of vendoring.
