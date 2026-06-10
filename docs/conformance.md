# JavaScript conformance: running chidori against Test262

Chidori executes agent code in an embedded QuickJS runtime (the in-repo
`chidori-quickjs` fork). To answer "is our JavaScript runtime at parity with
Bun and Node?" we run it against **Test262** — the official TC39 ECMAScript
conformance suite, and the one corpus that both Bun (JavaScriptCore) and Node
(V8) publish language-conformance numbers against. Test262 is therefore the
apples-to-apples yardstick; the runtime-specific suites (`Bun.serve`,
`node:test` internals, etc.) test product surface that does not generalize.

## TL;DR

```bash
# Vendor the suite (shallow clone of tc39/test262) and run language + built-ins:
scripts/test262.sh

# Run a subset:
scripts/test262.sh test/built-ins/Array
scripts/test262.sh --filter Promise

# Full machine-readable report:
scripts/test262.sh --json target/test262-report.json --verbose
```

The runner prints, e.g.:

```
Test262 (chidori/QuickJS bare context)
  pass 39178  fail 202  skip 7885  =>  99.49% of executed
```

## Current result

Against `test/language` + `test/built-ins` (scripts **and** modules):

| | pass | fail | skip | % of executed |
|---|---|---|---|---|
| chidori / QuickJS bare context | 39,178 | 202 | 7,885 | **99.49%** |

The remaining 202 failures are genuine QuickJS engine deviations that need
C-internals work, not harness or host fixes — see
[Remaining gaps](#remaining-gaps).

## What is measured, and why "bare context"

The runner drives the **bare ECMAScript context** — `SnapshotRuntime::new()` +
`new_context()` with *no* `chidori` host object and *no* captured-effect
prelude installed. That isolates pure language conformance, exactly how Bun and
Node report their Test262 numbers. Chidori's differentiators (security
sandboxing, deterministic captured effects, replay/snapshot) are layered *on
top of* this context; measuring the bare context first tells us whether the
language substrate is sound before the durability layer is added (see
[Security + replay parity](#security--replay-parity) below).

## Architecture

Two pieces:

1. **Conformance API on `chidori-quickjs`** (`crates/chidori-quickjs/src/lib.rs`)
   — a small public surface the harness needs that the durable runtime did not
   expose:
   - `SnapshotContext::eval_for_conformance(name, source, EvalMode)` — eval raw
     source (no module facade) and, on a throw, return a structured
     [`JsThrow`] carrying the thrown value's `name`/constructor (e.g.
     `"SyntaxError"`, `"Test262Error"`). That constructor name is what negative
     tests assert on; the durable path flattened exceptions to a single string.
   - `EvalMode` — `Script` / `StrictScript` / `Module` and their `Compile*`
     (parse-only) counterparts, mirroring Test262's `flags` and
     `negative.phase: parse`.
   - `SnapshotContext::run_pending_jobs()` — drain the microtask queue to settle
     async tests, surfacing a rejected job as a `JsThrow`.
   - `SnapshotContext::read_global_json(prop)` — read the captured `print()`
     buffer that async tests signal `$DONE` through.

   Plus two engine capabilities exposed through the `sys` FFI so the runner can
   exercise real behavior instead of stubs:
   - `JS_DetachArrayBuffer` — backs a real `$262.detachArrayBuffer`, so
     detached-buffer tests across DataView/TypedArray/ArrayBuffer run against the
     engine (took DataView and ArrayBuffer from many failures to 100%).
   - `JS_SetModuleLoaderFunc` — lets the runner register a filesystem module
     loader. The engine already parses `import()`; it just had no loader. With
     one, dynamic-import goes to 100% and `module`-flag tests (previously
     skipped wholesale) actually run.

2. **The runner** (`crates/test262-runner/`) — a standalone workspace binary
   that walks a Test262 checkout and, for each test file:
   - parses the `/*--- ... ---*/` YAML frontmatter (`flags`, `includes`,
     `negative`, `features`);
   - selects execution variants per the `flags` rules — `raw`; `module`;
     otherwise both `sloppy` and `strict` (honoring `onlyStrict` / `noStrict`);
   - spins up a **fresh runtime + context per variant** for isolation, installs
     a `print`/`$262` bootstrap plus the harness includes (`assert.js`,
     `sta.js`, `doneprintHandle.js`, and any `includes:`);
   - runs the body, handling:
     - **positive** tests — must not throw; `async` tests must emit
       `Test262:AsyncTestComplete` after jobs drain;
     - **negative parse** tests — compile-only; must throw the named error;
     - **negative runtime/resolution** tests — run (and drain jobs); must throw
       the named error;
   - reports `pass` / `fail` / `skip` per file, and writes an optional JSON
     report.

## Honest skips

The runner **skips** (does not count as failure) tests that require engine
features QuickJS does not implement or that the runner cannot host — the same
way Bun/Node skip what their engines lack. The list lives in
`UNSUPPORTED_FEATURES` in `crates/test262-runner/src/main.rs` (e.g. `Atomics`,
`SharedArrayBuffer`, `Temporal`, `decorators`, `iterator-helpers`,
`import-attributes`, `WeakRef`/`FinalizationRegistry`), plus:

- `intl402/` is skipped unless `--intl` is passed (no `Intl` in QuickJS).
- Agent tests (`CanBlockIsFalse`/`CanBlockIsTrue`) are skipped.

`module`-flag tests **run by default** (the registered module loader resolves
their fixture imports); pass `--no-modules` to skip them.

The headline percentage is `pass / (pass + fail)` over *executed* tests; the
skip count is reported alongside so the denominator is never hidden. When you
extend QuickJS to cover a skipped feature, delete its entry and the suite starts
holding it to account.

## Reproducibility

`scripts/test262.sh` shallow-clones `tc39/test262`. Set `TEST262_REF` to pin a
specific tag/commit for a reproducible number, and `TEST262_DIR` to point at an
existing checkout. The vendored suite (`vendor/test262/`, ~56k files) is
git-ignored.

## CLI reference

```
test262-runner [--test262 <dir>] [--filter <substr>] [--max <n>]
               [--json <out>] [--verbose] [--modules] [--intl] [paths...]
```

- `--test262 <dir>` — Test262 root (else `$TEST262_DIR`, else `vendor/test262`).
- `paths...` — files/dirs relative to the root (default `test/language` and
  `test/built-ins`).
- `--filter <substr>` — only run paths containing the substring.
- `--max <n>` — stop after `n` files (smoke runs).
- `--json <out>` — write a per-file JSON report.
- `--verbose` — print each failure with the thrown message.
- `--no-modules` — skip `module`-flag tests (they run by default).
- `--intl` — opt into `intl402` tests.

## Remaining gaps

The 202 residual failures are genuine QuickJS spec deviations requiring engine
(C) work — they are not host stubs or harness artifacts:

| count | area | nature |
|---|---|---|
| 80 | `RegExp/property-escapes` | incomplete Unicode `\p{…}` property tables |
| 23 | `String/prototype` | `Symbol.replace`/`match`/`search` & Unicode edge cases |
| ~25 | `TypedArray*` | resizable-ArrayBuffer / out-of-bounds tracking, species-ctor arg count |
| ~14 | async iteration | `AsyncFromSyncIterator`, `top-level-await` edge cases |
| 6 | `statements/with` | sloppy `with`-scope corner cases |
| 4 | `module-code/ambiguous-export` | QuickJS reports some unambiguous re-exports as ambiguous |
| ~50 | misc | `class` fields, `Object.defineProperty`, `Proxy`/`Reflect` corners |

Closing these means patching the vendored QuickJS fork (Unicode data,
resizable-buffer semantics, the regexp engine), which is deliberately out of
scope for the harness itself. Each is individually identifiable from the
`--json` report, so they can be picked off as engine work warrants.

### How the number got here

Honest gap-closing, not denominator games:

| step | pass% | fail | note |
|---|---|---|---|
| initial | 96.70% | 1,299 | scripts only; modules unmeasured |
| real `detachArrayBuffer` + skip unimplemented proposals | 98.83% | 452 | DataView/ArrayBuffer → 100%; proposals Bun/Node also skip |
| filesystem module loader + resolution-phase fix | **99.49%** | **202** | dynamic-import → 100%; modules now *executed*, not skipped |

## Security + replay parity

The bare-context number is the language baseline. Chidori's actual product
guarantee is "Bun/Node language behavior, **plus** our security sandbox and
deterministic replay." The next step in this harness is a `--prelude` mode that
installs the captured-effect prelude (deterministic `Date`/`Math.random`,
virtual timers, VFS-backed `node:fs`, captured crypto) *before* the test body,
and re-runs the same corpus. Any delta between bare and prelude runs is exactly
the conformance cost of the determinism layer — a regression budget we can watch
over time. That mode depends on the main `chidori` crate's prelude builder
(`src/runtime/typescript/snapshot.rs::snapshot_policy_prelude`) rather than just
`chidori-quickjs`, and is tracked as follow-up.
