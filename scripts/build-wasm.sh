#!/usr/bin/env bash
# Build the three sandbox WASM crates that are embedded via include_bytes! in
# src/runtime/sandbox.rs. Each lives in its own Cargo workspace so it can
# declare a no_std / WASI target without affecting the host build.
#
# Run this once before `cargo build` in the repo root, and re-run whenever
# you change code under sandbox-runtime/, sandbox-python/, or sandbox-js/.
# Tilt calls it automatically via a local_resource that watches those dirs.
#
# Targets:
#   sandbox-runtime  → wasm32-unknown-unknown  (no_std, bare memory ABI)
#   sandbox-python   → wasm32-wasip1           (RustPython, WASI stdio)
#   sandbox-js       → wasm32-wasip1           (Boa, WASI stdio)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

build() {
  local dir="$1"
  local target="$2"
  echo "==> building $dir ($target)"
  cargo build --release --target "$target" --manifest-path "$REPO_ROOT/$dir/Cargo.toml"
}

build sandbox-runtime wasm32-unknown-unknown
build sandbox-python  wasm32-wasip1
build sandbox-js      wasm32-wasip1

echo "==> sandbox WASM artefacts ready"
