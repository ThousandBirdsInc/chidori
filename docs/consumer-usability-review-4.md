# Consumer usability review, round 4: the day-2 surface

**Date:** 2026-07-17 · **Chidori:** 3.6.0, built from source at `6c4a50f` ·
**Perspective:** the same consumer as rounds
[1](./consumer-usability-review.md) · [2](./consumer-usability-review-2.md) ·
[3](./consumer-usability-review-3.md) — a developer whose provider is
DeepSeek — now past the honeymoon. Rounds 1–3 proved the engine records,
replays, and resumes. This round asks the questions that decide whether the
framework survives *month two* of a real project: can I use the npm
ecosystem the README promises? Does a run survive losing the machine, not
just the process? When a run fails on a bad model response, can I actually
*repair* it? And is "check in a checkpoint as a test" finally a workflow now
that `chidori verify` exists?

Surfaces exercised for the first time in this series: `chidori add` /
`install` / `remove` and real npm packages, `CHIDORI_RUN_STORE=sqlite` (the
durable mirror), machine-loss hydration, `CHIDORI_DURABILITY=strict` vs
`besteffort` under a dead mirror, `chidori resume --until-seq` time travel,
`CHIDORI_REPLAY_LAX`, and `chidori verify` as a CI gate.

## What was built

A **Supply-Chain Sentinel**
([`examples/supply-chain-sentinel/`](../examples/supply-chain-sentinel/)):
~250 lines of agent that audits this repository's own Rust dependencies,
live on `deepseek-v4-flash`:

1. parses `crates/chidori/Cargo.toml` + the workspace `Cargo.lock` with an
   npm TOML parser inside a `chidori.step` value checkpoint;
2. triages each direct dependency with a provider tool loop over two live
   tools — crates.io metadata and the OSV vulnerability database
   (`defineTool`, real `fetch` in both);
3. validates every model verdict with a schema library before trusting it;
4. gates publication on a human answer (`chidori.input` with the report as
   `details`);
5. publishes `AUDIT.md` via `workspace.write`.

The audit itself came back clean and accurate: all 10 audited crates
low-risk, six flagged as slightly behind latest (tokio 1.51.1 → 1.53.0,
axum 0.8.8 → 0.8.9, …), every version number spot-checked real.

## The numbers first

| Scenario | Result |
|---|---|
| `chidori add` of two packages, cold | **223ms** (verified, extracted, hardlinked — no Node anywhere) |
| First TOML parser tried (`smol-toml`) | **Fails at import**: cyclic ESM imports unsupported (Finding 3) — after `chidori add` said nothing and `chidori check` said `OK` |
| Second try (`confbox`) | **Fails at import**: `node:module` not shimmed; error names no file |
| `zod` — the docs' own flagship example — v4.4.3 *and* v3.25.76 | **Cannot run at all**: `ReferenceError: Cannot access binding before initialization`, blamed on the agent's line (Finding 4) |
| Third TOML parser (`fast-toml`) + `valibot` | Work. Package-shopping-by-trial-and-error is the real workflow |
| Full live audit, first attempt (SQLite mirror + strict durability on) | **Died at 97s, ~90% complete** — one verdict hit `maxTokens` mid-JSON; `format:"json"` correctly refused it, and the runtime warning named the seq and the fix |
| Repairing that run: strict resume with the fix | **Refused** — `max_tokens` is in every recorded prompt's args, so all 9 *good* verdicts diverge too (Finding 5) |
| Repairing with suggested `CHIDORI_REPLAY_LAX=1` | **Re-fails identically** — the truncated reply itself is served from cache |
| Repair that worked: `LAX + --until-seq 28 + --allow-source-change` | **Completed & published**, 81s — but 26 replayed / **78 re-paid live**, and the repaired run is **permanently unverifiable** (Finding 5) |
| Fresh clean run | 79s first try, ~95 records, publish gate answered over stdin |
| Replay of it, provider key deleted | **0.84s, $0, byte-identical** |
| `chidori verify` on it | **exit 0 in 0.52s** — "101 calls replayed, output identical — $0"; tampered source correctly refused via source hash |
| Machine loss (`rm -rf .chidori/runs/<id>`), completed run | **Hydrated back from `runs.sqlite3` in 0.6s** — `resume` and `trace` both |
| `kill -9` at 35s **plus** machine loss, then resume on "new machine" | **Completed**: hydrated, 30 replayed, 65 live, 61s — the strongest durability claim in the README, and it held (but see Finding 6) |
| Strict durability with an unreachable mirror | **Fail-fast before acting on the world**, error names the exact failed write; `besteffort` limps through as documented |
| Checkpoint-as-test fixture size for this modest agent | **24MB on disk** (~4× duplication of large values); `records.jsonl` alone 3.5MB, 312KB gzipped (Finding 7) |
| Whole campaign: 13 runs, 133 prompt calls, all scenarios | **$0.022 by chidori's meter / ~$0.04 by DeepSeek's billing** (balance 19.93 → 19.89) |

The engine half of the table is a clean sweep — round 4 found **zero
failures in the durable-storage layer**. Mirror, hydration, strict-mode
semantics, divergence detection, `verify`: every one behaved exactly as its
documentation says, including under `kill -9` plus a deleted run directory.
That is rare, and it is the reason to keep taking this framework seriously.

The consumer half is a different story, and it has a new theme. Rounds 1–3
were about the *engine's* failure paths. Round 4's failures are all in the
**ecosystem contract**: the README sells "npm packages without Node" and
"check in a checkpoint as a test," and both promises collapsed on first
contact — one on the second package I tried, the other on arithmetic
(24MB per fixture). Everything below was cheap to hit; most of it was found
in the first hour.

---

## Finding 1: one `file:` dependency bricks the entire package manager

The very first `chidori add zod smol-toml` failed with:

```
Error: resolving `@1kbirds/chidori@file:../../sdk/typescript` (required by the
project): file dependencies are not supported by chidori's package manager
```

The `package.json` contained one devDependency — `"@1kbirds/chidori":
"file:../../sdk/typescript"` — which is **exactly the layout this repo's own
round-3 example ships** (`examples/release-notes-concierge/package.json`),
and a completely ordinary way to get editor types in a monorepo. The
package-management doc does say file deps are "rejected with a clear error
rather than half-supported," and the error *is* clear. What it doesn't say
is the blast radius: rejection isn't per-dependency, it's **total**. `add`,
`install`, and `remove` all refuse to do anything at all — for *unrelated*
packages — until the offending line is deleted by hand.

A package manager that cannot coexist with the repo's own recommended
editor-types layout is a trap laid for every monorepo consumer. Skip the
unsupported dep with a warning (the way unresolvable `optionalDependencies`
already are) instead of halting the world.

## Finding 2: npm `@1kbirds/chidori@3.6.0` is not SDK 3.6.0

Having been forced off the `file:` dep, I did the natural thing:
`chidori add -D @1kbirds/chidori` (registry version: **3.6.0** — same
version as the binary I'd just built from this tag). Strict `tsc` then
rejected the agent on three counts:

```
error TS2305: Module '"chidori:agent"' has no exported member 'defineTool'.
error TS2339: Property 'util' does not exist on type 'Chidori'.
error TS2353: ... 'details' does not exist in type 'InputOptions'.
```

`defineTool` is the round-3 headline feature; `chidori.util.parallel` and
`input({ details })` are both documented in `llm.txt`. All three exist in
`sdk/typescript/src/` at this commit. The published 3.6.0 tarball predates
them: **same version number, different API surface.** The runtime accepted
the code the types rejected — so a newcomer without the repo checkout gets
red squiggles on the exact code the README teaches, and no version number
anywhere disagrees with any other.

Version-bump discipline (or CI that diffs the published `.d.ts` against the
source tree at release time) would make this impossible. Workaround used
here: a tsconfig `paths` override to the in-repo SDK source — available
only to someone who has the repo cloned.

## Finding 3: the compatibility cliffs are four, not three — and the fourth is invisible until runtime

`docs/package-management.md` names three compatibility cliffs (CJS
leaf-only, builtin allowlist, no native addons) and promises that `chidori
add` warns heuristically and `chidori check` "gives the definitive answer,
since the module graph resolves eagerly." My first package walked off a
**fourth, undocumented cliff**:

```
Error: node_modules/smol-toml/dist/struct.js: cyclic TypeScript imports are
not supported by the snapshot scaffold
```

`smol-toml` is pure-ESM, native-free, allowlist-clean — it passes all three
documented cliffs — and its parser has an ordinary, spec-legal import cycle
(`struct.js ⇄ extract.js`), as countless real parsers do. Three tools in a
row missed it:

- **`chidori add`**: no warning (the heuristic doesn't scan for cycles);
- **`chidori check`**: `OK: agent.ts` in 6ms — it resolves specifiers (it
  *does* catch a misspelled import) but never walks imports for cycles, so
  the doc's "definitive answer" claim is simply false today;
- **the error itself**: correctly names the file, but only at `chidori run`
  time — after everything upstream said yes.

The second attempt (`confbox`) failed the *documented* builtin cliff, which
would be fine — except the error was `unsupported node: builtin
'node:module'` with **no file, no importer, no chain**. In a dependency
tree, "something, somewhere, wants `node:module`" is a grep assignment, not
an error message.

## Finding 4: zod — the docs' own flagship example — cannot run, and the error blames *your* code

`docs/package-management.md` opens its "Using packages from agents" section
with `chidori add zod` and an `import { z } from "zod"` code sample. That
package does not work on this runtime. Not v4 (4.4.3, current), not v3
(3.25.76, the version era the docs were presumably written against):

```
Error: uncaught JavaScript exception
  × ReferenceError: Cannot access binding before initialization
   ╭─[zod_probe.ts:3:19]
 3 │ run(async () => { return z.object({ a: z.string() }).parse({ a: "hi" }); });
```

Root cause: zod's internal ESM import cycles get **past** the cycle
detector that caught smol-toml, load "successfully," and then evaluate in
an order the engine gets wrong — a TDZ explosion at first use. So the same
root cause (legal ESM cycles) produces two different behaviors: detected →
refused with a clear file name, or undetected → a runtime `ReferenceError`
whose stack points **at the consumer's own line**, with no hint that the
uninitialized binding lives three modules deep inside zod. I burned real
time suspecting my own code.

This is the single worst finding of the round, for three compounding
reasons: (a) zod is the de-facto standard schema library — it is the first
package a JS developer will reach for on exactly this framework's flagship
use case (validating LLM output); (b) it is the framework's *own
documented example*, which means nobody has ever run the docs' example
against the engine; (c) the failure mode is misattributed to the user.
Either make ESM cycle evaluation correct (it is spec-defined), or detect
*every* cycle and fail at `add`/`check` time with the file names. The
half-detector is the worst of both worlds.

(`valibot` worked first try and is a fine library — but "use valibot" is a
workaround, not an answer, and I found it by trial and error: five
`chidori add`s to get two working packages.)

## Finding 5: a run that failed on a bad model response cannot be cleanly repaired — and the one path that works forfeits `verify` forever

The first live audit died at 97s with one dependency's verdict truncated
mid-JSON by my too-small `maxTokens: 700`. Full credit first: the runtime's
handling of the *failure moment* is excellent. `format:"json"` refused to
return garbage as structure, and the warning is the best I've seen in this
series — it named the seq and the fix:

```
chidori: warning: prompt (seq 29) hit the 700-token output cap (stop reason
`length`) — the response is truncated mid-generation. Raise `maxTokens` ...
```

Then I tried to *repair* the run — ~90% of the work (9 of 10 verdicts, 60+
records) was journaled and paid for — and walked into a maze where every
door but one is locked, and the one that opens costs most of the run
anyway:

- **Fix + strict resume:** refused. `max_tokens` is part of every recorded
  prompt's args, so raising it diverges the nine *successful* cached calls,
  not just the failed one. Divergence at seq 5, precise and correct — and a
  dead end. The most common LLM fix there is (raising a token cap)
  invalidates the entire journal by construction.
- **Fix + `CHIDORI_REPLAY_LAX=1`** (what the error message suggests):
  re-fails **identically**. Lax mode serves the recorded *truncated* reply
  for seq 29 and my code re-throws on the same bad JSON. Unlike an actor
  `restart: "resume"`, a plain `chidori resume` never trims the trailing
  failed record — the poison is replayed as faithfully as the good calls.
- **The working path:** `CHIDORI_REPLAY_LAX=1 --until-seq 28
  --allow-source-change`. This completed and published. But `--until-seq`
  is a *prefix* operation, and with `util.parallel(concurrency: 3)` the
  interleaved journal put the bad record at seq 29 of 97 — so the repair
  replayed 26 records and **re-executed 78 live**, re-paying verdicts that
  were sitting fully-formed in the journal. The durability pitch
  ("never re-bill the same tokens") quietly inverts for repairs: the
  earlier the failure lands in an interleaved journal, the less the
  journal is worth.
- **The permanent scar:** the repaired run's journal is a chimera — old
  records say `max_tokens: 700`, the source now says 1600 — so
  `chidori verify` fails on it *forever* (divergence at seq 5, exit 1).
  Repair and checkpoint-as-test are mutually exclusive: to get a
  committable fixture I had to re-run the whole audit from scratch anyway.

What's missing is a first-class repair primitive: `resume --retry-failed`,
which trims the trailing failed record (the actor restart machinery already
knows how) and re-executes only it under the current source — with the
arg-divergence check scoped to the retried call. Every piece of this
exists in the codebase; it just isn't reachable from the CLI.

## Finding 6: silent degradation is still alive — a crash-resumed run published an empty report section

Round 3's thesis was "when the consumer misconfigures something, Chidori
succeeds silently with a degraded result." Round 4 reproduced it on the
flagship recovery path. The `kill -9` + machine-loss resume *completed* —
hydration perfect, 30 replayed, 65 live — and published `AUDIT.md` with an
**empty executive summary**: the summary prompt's live re-execution
returned `""` (the reasoning model burned the entire 500-token cap on
hidden reasoning; seq 93's recorded result is the empty string), and
nothing stood between an empty model reply and `workspace.write`.

Yes, my agent should have validated the summary the way it validated the
verdicts. But the asymmetry is the framework's: `format:"json"` prompts
fail loudly on truncation by default (`strict: true` — the round-3 fix),
while plain-text prompts return `""` with only a stderr warning that a
detached or CI run will never see. An opt-in `minTokens`/`nonEmpty` prompt
option — or simply promoting the truncation warning to a catchable error
when the visible output is empty — would close the last silent door on the
most common prompt kind there is.

## Finding 7: `chidori verify` is real now — but a checkpoint is 24MB, so "commit a checkpoint as a test" still isn't

The good news, and it is genuinely good: `chidori verify agent.ts <run>`
does exactly what round 3 asked for. Clean run → **exit 0 in 0.52s, zero
provider config, "output identical — $0"**. Tampered prompt text → refused
before replay via source hash. Divergent journal → exit 1 with the full
divergence report. It is a real CI gate and I trust it.

The bad news is the fixture itself. This is a modest agent — 10
dependencies, ~100 records — and its run directory weighs **24MB**:

```
7.3M  runtime.snapshot.json
6.9M  host_promises.json
6.3M  checkpoint.json
3.5M  records.jsonl        (312KB gzipped)
```

The cause is structural: every large value — the 112KB `Cargo.lock` read,
the parsed-manifest step result, every crates.io response — is stored
**~4 times**, once per artifact, uncompressed, inside JSON string
escaping. (The mirror then doubles it again: this session's `.chidori/`
totals 139MB for $0.04 of agent work.) Nobody commits a 24MB blob per
agent per source-revision to git — and "per source-revision" is load-bearing:
`verify`'s source-hash check means *any* edit, a comment included,
invalidates the fixture and demands a fresh live recording. The README's
"commit a checkpoint as a test" pitch needs either value-deduplication in
the store (one content-addressed blob referenced by all four artifacts —
the gzip ratio shows 90%+ is redundancy), or an explicit
`chidori export --fixture` that strips the snapshot/promise artifacts down
to what `verify` actually replays.

## Smaller notes

- `chidori stats` reports **`Tool calls: 0`** against a journal containing
  ~60 `defineTool` invocations — the counter predates the `mark`-based tool
  records and was never updated. The cost meter also read $0.022 where
  DeepSeek's billing said ~$0.04 (tool-loop turns and cache accounting on
  an OpenAI-compat provider seem undercounted) — directionally useful,
  not reconcilable.
- `chidori add` rewrites `package.json` with keys sorted alphabetically —
  `name` sinks below `dependencies`. Cosmetic, but it churns diffs in any
  repo with a formatting convention.
- `chidori stats` prices runs from `CHIDORI_PRICING` *at stats time* — run
  it without the env var and historical runs silently show
  `cost unknown`. The pricing table deserves to live in config, not in
  every shell that ever inspects a run.
- valibot's `peerDependencies: typescript` produces an unmet-peer warning
  on every install in a project that (by chidori's own pitch) needs no
  TypeScript compiler. A `chidori` install condition already exists for
  `exports` maps; peer warnings could respect the same reality.

## What worked — credit where due

The durable-storage layer is the most trustworthy thing this series has
tested, and it's not close:

- **`CHIDORI_RUN_STORE=sqlite` + hydration is flawless.** Delete the entire
  run directory; `resume` *and* `trace` silently materialize it back from
  `runs.sqlite3` and carry on — 0.6s to full replay. The README's hardest
  composite claim — `kill -9` mid-run, lose the disk, continue **live** on
  a new machine — worked on the first attempt, unrehearsed.
- **`CHIDORI_DURABILITY` semantics are exactly as written.** Strict with a
  dead mirror fails *before* acting on the world, naming the precise
  failed write; besteffort logs and limps. This is the difference between
  documentation and marketing, honored.
- **Divergence errors are the best in class.** Recorded args vs. current
  args, the differing field named, both digests, and a suggested escape
  hatch. Finding 5 is an argument about *policy*; the *mechanism* and its
  error reporting are superb.
- **The package manager is fast and honest when it can be**: 223ms cold
  installs, SHA-512 verification, a merge-friendly lockfile, and the
  CJS-leaf warning fired on exactly the package it applied to.
- **`chidori verify` exists and works** (Finding 7 is about the fixture
  economics, not the command), and the truncation warning that named seq
  29 and the fix is precisely the "fail loudly at the moment of failure"
  behavior rounds 1–3 kept asking for.
- **No secret ever touched a journal**: the provider key appears nowhere in
  24MB of run artifacts. Verified by grep, not by faith.

## Where this leaves a consumer

Rounds 1–3 established that the *engine* keeps its promises; round 4
establishes that the *storage layer under it* does too, all the way through
machine loss — that combination is genuinely rare, and for agents whose
dependencies are `fetch` and the standard library, I would ship this today.

What fails the month-two test is the ecosystem seam. "npm packages without
Node" currently means: the docs' flagship package throws a `ReferenceError`
attributed to your own code, the pre-flight tools vouch for packages they
cannot load, and the fourth compatibility cliff is documented nowhere. And
the two workflows a team would build its process around — repairing an
expensive failed run, and committing a checkpoint as a test — are each one
design decision away from real: a `--retry-failed` that trims the poisoned
record, and a fixture format that doesn't store every byte four times.

None of round 4's findings touch the architecture. Every one of them is a
seam between an excellent core and the ecosystem it advertises — which is
both the good news and the point: the durability engine has earned better
edges.

---

## Appendix: scenario commands

Everything below ran against `target/release/chidori` (3.6.0 at `6c4a50f`),
from `examples/supply-chain-sentinel/`, with:

```bash
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-…            # DeepSeek
export CHIDORI_RUN_STORE=sqlite CHIDORI_DURABILITY=strict
export CHIDORI_PRICING='{"deepseek-v4-flash":{"input_per_mtok":0.28,"output_per_mtok":0.42,"cache_read_multiplier":0.1}}'
```

```bash
# Packages (Findings 1–4)
chidori add zod smol-toml            # blocked by the file: dep, then 223ms
chidori add -D @1kbirds/chidori      # stale 3.6.0 types
chidori run …                        # smol-toml: cycle error; confbox: node:module;
                                     # zod v3+v4: TDZ ReferenceError; valibot+fast-toml: OK

# The live audit + repair maze (Finding 5)
printf 'publish\n' | chidori run agent.ts --trusted --model deepseek-v4-flash --input '{"top":10}'
chidori resume agent.ts <run> --trusted --allow-source-change             # divergence at seq 5
CHIDORI_REPLAY_LAX=1 chidori resume agent.ts <run> --trusted --allow-source-change   # re-fails on cached truncation
printf 'publish\n' | CHIDORI_REPLAY_LAX=1 chidori resume agent.ts <run> \
    --until-seq 28 --trusted --allow-source-change                        # 26 replayed / 78 live, published
chidori verify agent.ts <run>                                             # exit 1 forever (chimera journal)

# The clean fixture + CI gate (Finding 7)
printf 'publish\n' | chidori run agent.ts --trusted --model deepseek-v4-flash --input '{"top":10}'
unset CHIDORI_OPENAI_COMPAT_URL CHIDORI_OPENAI_COMPAT_KEY
chidori resume agent.ts <run>        # 0.84s, $0, byte-identical
chidori verify agent.ts <run>        # exit 0, 0.52s
sed -i 's/Risk rubric/Scoring rubric/' agent.ts && chidori verify …       # refused, source hash

# Machine loss (the flagship, twice)
rm -rf .chidori/runs/<run> && chidori resume agent.ts <run>               # hydrated, 0.6s
kill -9 <pid mid-run> && rm -rf .chidori/runs/<run> \
  && printf 'publish\n' | chidori resume agent.ts <run> --trusted         # 30 replayed / 65 live, completed
                                                                          # …with an empty summary (Finding 6)

# Durability postures against a dead mirror
CHIDORI_RUN_STORE=s3://no-such-bucket CHIDORI_RUN_STORE_ENDPOINT=http://127.0.0.1:9 \
  CHIDORI_DURABILITY=strict   chidori run …   # fail-fast, names the write
  CHIDORI_DURABILITY=besteffort chidori run … # completes
```
