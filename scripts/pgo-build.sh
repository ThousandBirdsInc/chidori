#!/usr/bin/env bash
# Profile-guided-optimization build of the chidori-js `run` benchmark binary.
#
# An interpreter dispatch loop is the textbook PGO beneficiary: execution is
# dominated by indirect branches and dense, branchy op bodies, and PGO's
# branch-layout / hot-cold-splitting decisions typically buy 5-15% wall-clock
# on exactly this shape of code — on top of the LTO + codegen-units=1 the
# release profile already has. Instruction *counts* (callgrind) barely move;
# the win is branch prediction and icache locality, so measure it with
# wall-clock (benchmarks/run.mjs), not callgrind.
#
# Three stages:
#   1. build instrumented (-Cprofile-generate), in its own target dir so the
#      instrumented artifacts never pollute the normal build cache;
#   2. run the cross-runtime workload corpus to collect .profraw;
#   3. merge with llvm-profdata and rebuild with -Cprofile-use.
#
# Requires the rustup `llvm-tools` component (for llvm-profdata):
#   rustup component add llvm-tools
#
# Usage:
#   scripts/pgo-build.sh [output-path]
# The optimized binary lands at target/pgo/release/examples/run (and is copied
# to [output-path] if given). Point the benchmark harness at it with:
#   node crates/chidori-js/benchmarks/run.mjs --no-build --chidori-bin target/pgo/release/examples/run
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

WORKLOADS="$REPO_ROOT/crates/chidori-js/benchmarks/workloads"
PGO_DIR="$REPO_ROOT/target/pgo-data"
TARGET_DIR="$REPO_ROOT/target/pgo"
# Extra cargo args (e.g. --features mimalloc) can be passed via PGO_CARGO_ARGS.
# The benchmark harness builds with mimalloc, so default to matching it.
CARGO_ARGS=(${PGO_CARGO_ARGS---features mimalloc})

SYSROOT="$(rustc --print sysroot)"
LLVM_PROFDATA="$(find "$SYSROOT" -name llvm-profdata | head -1)"
if [[ -z "$LLVM_PROFDATA" ]]; then
    echo "llvm-profdata not found; install it with: rustup component add llvm-tools" >&2
    exit 1
fi

rm -rf "$PGO_DIR"
mkdir -p "$PGO_DIR"

echo "== stage 1/3: building instrumented binary"
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
    cargo build --release -q -p chidori-js "${CARGO_ARGS[@]}" --example run \
    --target-dir "$TARGET_DIR"

echo "== stage 2/3: collecting profiles over the workload corpus"
for wl in "$WORKLOADS"/*.js; do
    # startup.js is the startup-baseline probe, but include it too: real
    # invocations pay realm setup, so its branches deserve profile weight.
    echo "   $(basename "$wl")"
    "$TARGET_DIR/release/examples/run" "$wl" > /dev/null
done

"$LLVM_PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

echo "== stage 3/3: rebuilding with profile feedback"
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata" \
    cargo build --release -q -p chidori-js "${CARGO_ARGS[@]}" --example run \
    --target-dir "$TARGET_DIR"

BIN="$TARGET_DIR/release/examples/run"
echo "PGO-optimized binary: $BIN"
if [[ $# -ge 1 ]]; then
    cp "$BIN" "$1"
    echo "copied to: $1"
fi
