# Node.js core test suite — vendored compatibility subset

The `suite/` directory contains unmodified test files from the Node.js
project (`nodejs/node`, `test/parallel`), pinned to the release recorded in
`NODE_VERSION` and downloaded by `scripts/vendor-node-compat-tests.sh`. They
are the compatibility yardstick for chidori's `node:` builtin shims — the
same corpus Bun runs wholesale and Deno vendors under `tests/node_compat`.

Node.js is MIT-licensed; individual files carry their original Joyent/Node
contributors license headers. The files are test fixtures, not part of the
chidori runtime, and are excluded from published packages.

## How they run

`crates/chidori/src/node_compat.rs` wraps each CommonJS test as a chidori
agent module (an in-scope `require()` over the builtin shims plus a minimal
reimplementation of the suite's `common` helpers), executes it on the real
engine, and compares per-file outcomes against `expectations.json`:

```
cargo test -p chidori --lib -- node_compat            # gate against expectations
NODE_COMPAT_UPDATE=1 cargo test -p chidori --lib -- node_compat
                                                      # refresh expectations +
                                                      # docs/node-compat-report.md
```

A `pass` means every assertion in the vendored Node test held. A `fail` is a
real compatibility gap (tracked in `docs/node-compat-report.md`). A `skip`
means the test needs a Node test-suite facility the harness does not emulate
(fixtures, tmpdir).

## Growing the suite

Add candidate filenames to `scripts/vendor-node-compat-tests.sh`, re-run it,
then refresh expectations with `NODE_COMPAT_UPDATE=1`. Absent filenames 404
harmlessly, so the candidate list can carry guesses.
