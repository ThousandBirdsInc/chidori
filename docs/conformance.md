# JavaScript conformance: running chidori against Test262

Chidori executes agent code on its **pure-Rust JavaScript engine**
(`crates/chidori-js`, oxc parser, zero `unsafe`) — now the only JS engine in
the tree. To answer "is our JavaScript runtime at parity with Bun and Node?" we
run it against **Test262**, the official TC39 ECMAScript conformance suite and
the one corpus that both Bun (JavaScriptCore) and Node (V8) publish
language-conformance numbers against. Test262 is therefore the apples-to-apples
yardstick; runtime-specific suites (`Bun.serve`, `node:test` internals, etc.)
test product surface that does not generalize.

Because `chidori-js` has no fallback engine anymore, conformance is now
load-bearing: a language regression directly breaks real agents. CI gates every
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
  pass 39361  fail 413  skip 7517  =>  98.96% of executed
```

## Current result

Against `test/language` + `test/built-ins` (scripts **and** modules), at the
pinned suite commit:

| | pass | fail | skip | % of executed |
|---|---|---|---|---|
| chidori pure-Rust engine, bare context | 39,361 | 413 | 7,517 | **98.96%** |

The headline percentage is `pass / (pass + fail)` over *executed* tests; the
skip count is reported alongside so the denominator is never hidden.

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
import by policy — `import()` rejects with a TypeError, as before.

## Parallel execution

The runner fans the per-file loop out across **one worker per CPU** by default
(override with `TEST262_JOBS`). Workers pull file indices off a shared atomic
cursor — dynamic load-balancing, because second-level directories vary by orders
of magnitude in cost — and each holds its own harness cache. Per-test
timeout/panic isolation is unchanged: every execution still runs on its own
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
**every** object the VM ever allocated (including orphaned cycles the old
realm-root walk missed), and `Vm::collect_cycles()` offers mark-sweep for
long-lived VMs. Since the runner disposes a fresh VM per test, memory across a
single-process run is now flat (~20 MB RSS over the 21k `language/` tests;
it previously grew without bound, ~300 MB over `built-ins/Array` alone).

Both `scripts/test262.sh --gate` and `--update-baseline` still run the suite
**one second-level directory at a time, in a fresh process each** — no longer
for memory, but for crash isolation: a single engine abort (e.g. a stack
overflow on a pathological test) kills only its chunk, not the whole sweep.
The runner's `--state <file>` flag merges per-test results across chunks;
`--baseline <file>` gates each chunk against the full baseline. Each chunk now
runs its own files in parallel across all cores (see [Parallel
execution](#parallel-execution)), so a full chunked pass is several times faster
than the old ~24-minute single-threaded sweep — scaling with the core count of
the box (or CI runner) it lands on.

## Honest skips

The runner **skips** (does not count as failure) tests that require features the
engine intentionally does not implement — the same way Bun/Node skip what their
engines lack. The list lives in `UNSUPPORTED_FEATURES` in
`crates/test262-runner/src/main.rs` (e.g. `Temporal`, `decorators`,
`iterator-helpers`, `import-attributes`, `WeakRef`/`FinalizationRegistry`), plus
`intl402/` (skipped unless `--intl`) and the agent (`CanBlock`, and the
`atomicsHelper.js` multi-agent harness) tests. When the engine grows to cover a
skipped feature, delete its entry and the suite starts holding it to account.

`SharedArrayBuffer` and `Atomics` **are** implemented (so their feature tags are
no longer skipped). The embedded runtime is single-threaded — the engine is
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

Not yet implemented (so still failing/skipped under `--intl`): the other
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
the `icu_locale` canonicalizer does not apply. `intl402/` remains skipped in
the default gate (it is opt-in via `--intl`), so this surface is not yet part
of the committed baseline.

## Temporal (opt-in: `--temporal`)

The TC39 Temporal proposal is being implemented incrementally on top of
[`temporal_rs`](https://crates.io/crates/temporal_rs) (the proposal's Rust
reference implementation: ISO-calendar arithmetic, durations, rounding, time
zones). Each `Temporal.*` instance stores its backing `temporal_rs` value in an
`Internal::Temporal` slot (a GC leaf — no JS references).

All eight Temporal types are implemented, plus `Temporal.Now`. Against the
full `test/built-ins/Temporal` tree (run with `--temporal`) the engine passes
**3,886** of 4,603 executed tests (**84.4%**), from zero. Per type:

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
`Temporal.Now` reads the system clock (the bare conformance context, like
`Date`; the durable runtime captures it as an effect at a higher layer).

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
fails or disappears. Newly *passing* tests never break the build; they print a
hint to refresh the baseline. After an intentional conformance change, run
`scripts/test262.sh --update-baseline` and commit the diff (each flipped test is
a single readable line in review).

## Remaining gaps

The residual failures, by area (top clusters of the ~413 total). Strings are now
WTF-8-backed with full UTF-16 code-unit semantics — `.length`/indexing/iteration,
the `String.prototype` surface, the RegExp matcher (non-unicode per code unit,
unicode per code point), lone-surrogate subjects and patterns, literals,
`decodeURI`, and `iu` case folding — so the former "scalar-string model" clusters
are largely cleared; the counts below shift accordingly (refresh from a `--json`
report before targeting):

| area | nature |
|---|---|
| `built-ins/RegExp` | `v`-flag set operations (`unicodeSets`); `\p{...}` property-of-strings; `prototype` long tail |
| `language/expressions` | dynamic-`import()` semantics and the last class/eval corners |
| `language/statements` | labelled/eval interplay and remaining class corners |
| `built-ins/Object` | sparse indices beyond the dense cap; descriptor corners |
| `language/module-code` | namespace internals, hoisted default-function exports, TLA ordering |
| `language/eval-code` | eval-created global binding attributes, lexical/var collisions |
| `built-ins/Array` | sparse indices beyond the dense cap |
| `built-ins/String` | full Unicode case folding (`fold` is a simple-fold approximation) |
| `built-ins/TypedArray` | `subarray`/`set` corners on resizable buffers |
| `language/global-code` | global lexical/var binding interactions |
| `language/literals/regexp` | `eval("/"+fromCharCode(cu)+"/").source` round-trip — a UTF-8 (oxc) front-end limit; `String.fromCharCode` is kept lossy until regex-/eval-source byte-span fidelity lands |
| `built-ins/ArrayBuffer` | resizable-buffer `slice`/transfer corners |

(Recent sweeps: the dynamic-`import()`/`with`-scope work cleared 268
failures; the derived-class construction model — `super()` as a real
`Construct`, `this`-TDZ, builtin subclassing, `new.target`-derived
prototypes, class constructors uncallable without `new` — cleared another
144; and the 2026-06-12 batch — named function/class self-bindings, a real
`arguments` exotic object, %ThrowTypeError% restricted properties,
object-literal `__proto__`, computed-key SetFunctionName, and `delete`
identifier semantics — cleared 194 more. The legacy normative-optional
`caller` feature is now an honest skip. Explicit resource management —
`using`/`await using` with spec disposal on every exit path, awaited
async disposal, SuppressedError chaining, and per-iteration `for-of`
disposal — landed next and cleared its entire 66-test cluster, +68 with
the `for (const …)` immutability fix it exposed. The Array sweep followed:
spec-generic `Get`/`Set` loops with 2^53-range index arithmetic across
`Array.prototype`, proxy-piercing `IsArray`, species in `flat`/`flatMap`,
spec change-array-by-copy semantics, `@@unscopables`, a dense-array own
`length`/element fix in the engine's `[[Set]]` path, and a
`Reflect.set`-with-receiver fix — +104, taking `built-ins/Array` from 136
failures to 40. Most recently, a class/private-elements rewrite — a real
spec PrivateEnvironment (fresh per-evaluation Private Names in a runtime
`[[PrivateElements]]` side table, not `#name@id` string keys), so brand
checks, multiple class evaluations, private methods/accessors on proxies,
`#x`-visible-to-direct-eval, and the non-extensible/double-init TypeErrors
all hold; spec super property references (`super.x`/`super[e]` read, write,
compound-assign and update through `[[HomeObject]]` + GetSuperBase, with
the derived-ctor `this`-TDZ and `delete super.x` ReferenceError); class
static blocks and the deferred static-initializer phase; class bodies as
strict-mode code; object-rest `CopyDataProperties` with computed
excluded-key ordering; a `String.prototype` sweep (`normalize`/
`localeCompare` via `unicode-normalization`, ToInteger/ToString coercion
order across the search/pad methods, `replaceAll` empty-pattern, the
`match`/`matchAll`/`search` RegExpCreate+Invoke fallback, `thisStringValue`
for `toString`/`valueOf`); `Object.freeze`/`seal`/`isFrozen`/`isSealed`
rebuilt on spec SetIntegrityLevel/TestIntegrityLevel (proxy-aware); Date
setters that no longer clobber a `valueOf`-mutated invalid date and
sign-padded negative years; and the `delete` operator's nullish-base
ToObject TypeError, non-deletable array `length`, and non-reference operand
evaluation — together taking the suite from 757 to 492 failures
(98.10% -> 98.76%). Tagged templates followed: a per-source-position cache
(keyed on the function proto, the spec's Parse Node identity) of frozen
template objects, with a separate frozen `raw` array, `undefined` cooked
values for illegal escapes, and `SetIntegrityLevel` taught to freeze an
array's derived `length` — clearing the tagged-template cluster and a few
array-freeze corners, 492 -> 466 (98.83%). Then JSON and Proxy: proxy-aware
`JSON.stringify` (IsArray pierces proxies, array length/elements and object
keys go through traps), `toJSON`/boxed-`BigInt` handling; and a batch of
Proxy fixes — strict assignment through a proxy throws on a `false` `[[Set]]`,
trap-less internal methods forward through the target's own internal method
(so a revoked proxy target throws), `Object.preventExtensions` throws on a
`false` result, a proxy captures its callability at creation (`typeof` of a
revoked function proxy stays `"function"`), and `instanceof` walks the
prototype chain through proxy `[[GetPrototypeOf]]` traps — 466 -> 445
(98.88%). Then global function declarations: a `$262.evalScript` host hook,
spec CanDeclareGlobalFunction checks run before any binding is created
(so a non-definable name aborts instantiation without leaking a `var`), and
CreateGlobalFunctionBinding gives the global property the right
writable/enumerable/configurable attributes — 445 -> 432 (98.91%). Then
`ArrayBuffer.prototype.slice` was made species-aware (SpeciesConstructor +
the result-buffer validations) and the constructor's `options` argument
ignores a non-object value instead of throwing — 432 -> 422 (98.94%). Then
`Object.prototype.isPrototypeOf` (non-object arg returns false before
ToObject(this); the chain walk follows proxy `[[GetPrototypeOf]]`) and
`[[OwnPropertyKeys]]` surfacing an array's / String object's `length`
(so `getOwnPropertyNames`/`Reflect.ownKeys` include it in order) — 422 ->
415 (98.96%). A String exotic object's in-range integer index is now a
non-writable own property for `[[Set]]` (so `Object.assign` onto a string
throws), and `%TypedArray%.prototype.toString` is the same function object
as `%Array%.prototype.toString` — 415 -> 413.)

Each failure is individually identifiable from a `--json` report, so the
clusters can be picked off as engine work warrants.

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
               [--verbose] [--no-modules] [--intl] [paths...]
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

Environment:

- `TEST262_JOBS` — parallel workers (default: one per CPU). `1` forces a serial
  run; results are identical either way.
- `TEST262_TIMEOUT_MS` — per-test wall-clock budget (default 5000). The gate
  scripts pin this so the committed baseline stays reproducible.
- `TEST262_DIR` — an existing Test262 checkout to use instead of vendoring.
