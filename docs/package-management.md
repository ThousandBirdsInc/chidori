# Package management

Chidori ships a native npm package manager — `chidori add`, `chidori install`,
`chidori remove` — so agents can use packages from the npm registry without
Node, npm, or bun installed. It follows the same design that makes modern
package managers like bun and pnpm fast:

- **Content-addressed global store.** Every package version is downloaded,
  verified, and extracted exactly once per machine, into
  `~/.chidori/cache/packages/<integrity-hash>/`. Projects never hold their own
  copy of file contents.
- **Hardlink materialization.** `node_modules` is assembled by hardlinking
  files out of the store (copying only when linking fails, e.g. across
  filesystems). Warm installs are offline and take milliseconds — the smoke
  benchmark installs a 3-package tree in ~1ms warm vs ~250ms cold.
- **SHA-512 verification.** Every tarball is checked against the registry's
  `sha512` subresource integrity before it can enter the store. Packages that
  publish only a legacy `sha1` shasum (pre-2017 publishes) are refused by
  default — SHA-1 is collision-broken — unless `CHIDORI_PKG_ALLOW_SHA1=1`
  explicitly opts in. Hashing and extraction run on blocking worker threads,
  off the download path.
- **Sorted JSONL lockfile.** `chidori.lock.jsonl` holds one JSON object per
  line, strictly sorted (name, then semver). Two branches adding different
  dependencies touch disjoint lines, so git merges apply cleanly instead of
  conflicting inside one large JSON document.
- **No lifecycle scripts.** `preinstall`/`postinstall`/`prepare` never run.
  Installs are pure data movement, which closes off the largest npm
  supply-chain attack vector. Agent code runs on chidori's embedded engine, so
  native `node-gyp`-style builds don't apply.

## Commands

```bash
chidori add zod                 # resolve latest, write ^range to package.json
chidori add zod@^3.22.0         # explicit range
chidori add left-pad@1.3.0      # exact version
chidori add @scope/pkg@beta     # dist-tags work
chidori add -D typescript       # devDependencies
chidori install                 # from the lockfile (offline when warm)
chidori install --frozen        # CI: fail instead of re-resolving on drift
chidori remove zod              # manifest + lockfile + node_modules (offline)
```

Environment overrides:

| Variable | Effect |
| --- | --- |
| `CHIDORI_NPM_REGISTRY` | Registry base URL (mirrors, private registries, tests) |
| `CHIDORI_PKG_CACHE_DIR` | Store location (default `~/.chidori/cache/packages`) |

## How installs work

1. **Resolve.** Requested ranges (npm semver: `^`, `~`, `1.x`, hyphen ranges,
   `||`, dist-tags) are resolved breadth-first against abbreviated registry
   metadata (`application/vnd.npm.install-v1+json`). Versions already pinned
   in the lockfile are preferred when they still satisfy, so adding one
   package doesn't churn unrelated pins. The highest satisfying version wins
   otherwise. `optionalDependencies` are skipped (with a warning) when they
   fail to resolve; unmet `peerDependencies` warn but don't auto-install.
2. **Plan.** The resolved set is laid out npm-style: root dependencies own the
   top of `node_modules`, shared transitive dependencies hoist to the top, and
   version conflicts nest under their dependent
   (`node_modules/a/node_modules/b`). The plan is exactly what the runtime's
   Node-style resolver (`runtime/typescript/resolver.rs`) expects to walk.
3. **Fetch.** Only store misses are downloaded (up to 8 tarballs
   concurrently), verified, and extracted into the store atomically (temp dir
   + rename, so racing installs can't corrupt an entry).
4. **Materialize + prune.** Each planned location is hardlinked from the
   store; a location already holding the right `name@version` is left
   untouched. Anything in `node_modules` the plan doesn't claim is removed —
   `node_modules` is fully machine-managed.

`chidori remove` and an in-sync `chidori install` never touch the network: the
lockfile carries exact versions, dependency edges, tarball URLs, and integrity
hashes, so the tree rebuilds from the store alone.

## Using packages from agents

Installed packages import the way they would under node or bun:

```ts
import { run } from "chidori:agent";
import { z } from "zod";
import ms from "ms";

run(async (input: { minutes: number }) => {
  const schema = z.object({ minutes: z.number() });
  return { human: ms(schema.parse(input).minutes * 60_000, { long: true }) };
});
```

The module loader resolves bare specifiers through the full Node ESM
algorithm: `exports` maps (with a `chidori` condition packages can target),
`main` fallback, subpaths, scoped packages, and nested `node_modules`
shadowing. ESM builds are preferred via the `import`/`module` conditions.

CommonJS support is deliberately minimal: a *leaf* CJS file (no `require`
calls at runtime) is wrapped so its `module.exports` becomes the default
export — enough for classics like `ms` or UMD single-file bundles. A CJS file
that calls `require()` throws with a clear message; prefer packages that ship
an ESM build (most modern ones do). JSON subpath imports resolve to a default
export.

## Compatibility

Chidori's embedded engine is **not Node**, and "no Node required" cuts both
ways: a package that works under `node` is not automatically compatible. The
three cliffs, concretely:

1. **CommonJS is leaf-only.** ESM builds load fully. A CommonJS file loads
   only if it never calls `require()` at runtime (the wrapper turns its
   `module.exports` into the default export). Any runtime `require()` throws —
   at *import time*, not install time.
2. **Node builtins are a small allowlist.** The runtime shims exactly:
   `process`, `buffer`, `util`, `fs`, `fs/promises`, `crypto`, `http`,
   `https`, `path`, `path/posix`, `events`, `url`, `assert`,
   `assert/strict`, `os`. Anything else — `net`, `stream`, `child_process`,
   `worker_threads`, `zlib`, `tls`, `vm`, … — is not provided, and importing
   it fails with an allowlist error. (Networking is `fetch` plus the
   `node:http`/`node:https` client shims, all captured for replay.)
3. **No native addons.** There is no node-gyp build step (lifecycle scripts
   never run) and no way to load a `.node` binary. Packages that depend on
   `node-gyp-build`, `prebuild-install`, `bindings`, etc. cannot work, even
   when the install "succeeds".

In short: **pure-ESM, native-free, allowlist-respecting packages work**; the
rest fail at first import. Because installs are pure data movement, `chidori
add` can't make an incompatible package fail to install — instead it scans
each added package for these three cliffs and prints a `warning:` when it
finds one (no ESM build, native-addon markers, imports of unprovided
builtins). The scan is a bounded heuristic: it can miss dynamically computed
specifiers, so a quiet `add` is strong — not perfect — evidence of
compatibility. `chidori check agent.ts` (or just running the agent) gives the
definitive answer, since the module graph resolves eagerly.

## Out of scope (v1) and why

- **Lifecycle scripts** — by design, see above. Not "not yet": running
  arbitrary registry-supplied shell on `install` is incompatible with
  chidori's sandbox posture.
- **`node_modules/.bin` linking** — chidori doesn't execute package binaries;
  there's no Node process to run them.
- **git / file / workspace / `npm:` alias dependencies** — not resolvable
  from the registry. Explicitly `chidori add`ing one is a clear error.
  *Pre-existing* manifest entries in these forms are skipped per-dependency
  with a warning instead of blocking the project: `add`/`install`/`remove`
  proceed for everything else, package.json keeps the entry verbatim, and a
  `node_modules` entry another tool materialized for it is never pruned.
- **Full CommonJS emulation (`require`)** — would need a synchronous module
  linker path in the engine; revisit if real agent dependencies demand it.
- **Auto-installed peer dependencies** — warned instead; install explicitly.

## Comparison notes (bun, pnpm)

This design deliberately matches the properties that make modern single-binary
package managers fast and safe — content-addressed store, link-based
materialization, SHA-512 verification off the hot path, merge-resistant JSONL
lockfile. Capabilities those toolchains have that chidori intentionally does
*not* take from the package manager:

- **Sandboxed execution of untrusted packages** (`bunx`-style exec modes):
  chidori already has a stronger equivalent at the runtime layer — the
  deny-by-default `--untrusted` policy profile and OS-level `--isolate`
  process sandbox apply to *all* agent code, packages included, not just a
  special exec mode.
- **Parse-once toolchain** (one AST feeding runtime/linter/formatter): chidori
  already parses with oxc once per module on the load path; lint/format are
  editor/CI concerns out of chidori's scope.
- **Test runner / bundler / typechecker**: `chidori check` covers agent
  validation; a full JS toolchain is a non-goal.
