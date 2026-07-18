# chidori-wasm

The chidori-js engine — bytecode compiler, VM, GC, builtins, and the durable
replay runtime — compiled to WebAssembly and driven from a browser page.

The engine crate is pure Rust (`#![forbid(unsafe_code)]`, no C, no threads, no
filesystem), so it builds for `wasm32-unknown-unknown` unmodified. This crate
adds only the boundary: a `wasm-bindgen` wrapper around
`chidori_js::replay::ReplayRuntime` that exposes the record/replay pump to
JavaScript.

## The pump protocol

The page owns the event loop and the host effects (fetch, time, randomness,
prompts). The runtime is a pump:

```js
const rt = new WasmRuntime(bundle, ['now', 'random', 'httpFetch']);
for (;;) {
  const status = JSON.parse(rt.runUntilBlocked());
  if (status.status === 'completed') break;
  // status: { status: 'blocked', opId, name, args }
  const result = await host[status.name](...status.args);   // e.g. fetch()
  rt.resolveOp(status.opId, JSON.stringify(result));        // journaled
}
const blob = rt.toBlob();   // durable artifact: bundle + effects + journal
```

`WasmRuntime.fromBlob(blob)` restores in replay mode: journaled effects are
served from the journal (no network, no reruns of anything non-deterministic),
and the pump only surfaces ops past the recorded frontier — so a suspended run
can resume in a fresh tab, or a completed run can replay with byte-identical
output and zero live host calls. The blob is the same `DurableBlob` artifact
the native runtime uses.

## Build and run the demo

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli   # version must match Cargo.toml's wasm-bindgen pin

scripts/build-wasm.sh
python3 -m http.server -d crates/chidori-wasm/www 8080
# open http://localhost:8080 — record a run, then replay it offline
```

## What stays native

The `chidori` CLI crate is deliberately not part of the wasm build: it is the
*host* side — tokio, reqwest, axum, SQLite session stores, OS sandboxing
(seccomp/Landlock) — and in the browser those responsibilities belong to the
page (fetch, IndexedDB/localStorage, the browser sandbox). The engine and the
journal format are shared; the host is swapped.

## Tests

The driver core is plain Rust (`src/lib.rs`, `driver` module), so the full
record → suspend → resolve → replay cycle runs under `cargo test -p
chidori-wasm` on the native target; browser behavior is exercised by the demo
page.
