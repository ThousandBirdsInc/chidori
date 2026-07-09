#!/usr/bin/env bash
# Profile-guided-optimization builds.
#
# An interpreter dispatch loop is the textbook PGO beneficiary: execution is
# dominated by indirect branches and dense, branchy op bodies, and PGO's
# branch-layout / hot-cold-splitting decisions stack on top of the release
# profile's fat LTO + codegen-units=1. Measured on the cross-runtime suite:
# -15.5% wall-clock geomean vs the plain release build, no workload regressed
# (see crates/chidori-js/benchmarks/README.md, "Build variants"). Instruction
# *counts* (callgrind) barely move; the win is branch prediction and icache
# locality, so measure PGO with wall-clock, never callgrind.
#
# Two modes:
#   scripts/pgo-build.sh [output-path]
#       (default) builds the chidori-js `run` example, training on the
#       cross-runtime benchmark workloads. For benchmarking the engine.
#   scripts/pgo-build.sh --bin chidori [--target <triple>] [--locked] [output-path]
#       builds the shipped `chidori` CLI binary, training on offline example
#       agents plus scripts/pgo-train/train.ts (a deterministic agent that
#       mirrors the benchmark suite's interpreter hot paths inside a real
#       agent run). Used by the release workflow. --target must be a triple
#       the build host can execute (the training runs the instrumented
#       binary); the release workflow therefore skips PGO for the
#       cross-compiled x86_64-apple-darwin target.
#
# Three stages either way:
#   1. build instrumented (-Cprofile-generate), in its own target dir so the
#      instrumented artifacts never pollute the normal build cache;
#   2. run the training corpus to collect .profraw;
#   3. merge with llvm-profdata and rebuild with -Cprofile-use.
#
# Requires the rustup `llvm-tools` component (for llvm-profdata):
#   rustup component add llvm-tools
#
# Extra cargo args (e.g. --features mimalloc) can be passed via PGO_CARGO_ARGS.
# Default: none — PGO over the stock (glibc-malloc) build measured fastest
# (-15.5% geomean vs release; PGO+mimalloc only reached -10.2%).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

MODE="example"
TARGET=""
OUT=""
CARGO_ARGS=(${PGO_CARGO_ARGS-})
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin)
            [[ "${2:-}" == "chidori" ]] || { echo "only '--bin chidori' is supported" >&2; exit 1; }
            MODE="chidori"; shift 2 ;;
        --target)
            TARGET="$2"; shift 2 ;;
        --locked)
            CARGO_ARGS+=("--locked"); shift ;;
        *)
            OUT="$1"; shift ;;
    esac
done

PGO_DIR="$REPO_ROOT/target/pgo-data"
TARGET_DIR="$REPO_ROOT/target/pgo"
TARGET_ARGS=()
TARGET_SUBDIR=""
if [[ -n "$TARGET" ]]; then
    TARGET_ARGS=(--target "$TARGET")
    TARGET_SUBDIR="$TARGET/"
fi

if [[ "$MODE" == "chidori" ]]; then
    BUILD_ARGS=(-p chidori --bin chidori)
    BIN="$TARGET_DIR/${TARGET_SUBDIR}release/chidori"
else
    BUILD_ARGS=(-p chidori-js --example run)
    BIN="$TARGET_DIR/${TARGET_SUBDIR}release/examples/run"
fi

SYSROOT="$(rustc --print sysroot)"
LLVM_PROFDATA="$(find "$SYSROOT" -name llvm-profdata | head -1)"
if [[ -z "$LLVM_PROFDATA" ]]; then
    echo "llvm-profdata not found; install it with: rustup component add llvm-tools" >&2
    exit 1
fi

rm -rf "$PGO_DIR"
mkdir -p "$PGO_DIR"

echo "== stage 1/3: building instrumented ${MODE} binary"
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
    cargo build --release -q "${BUILD_ARGS[@]}" "${CARGO_ARGS[@]}" "${TARGET_ARGS[@]}" \
    --target-dir "$TARGET_DIR"

echo "== stage 2/3: collecting profiles over the training corpus"
if [[ "$MODE" == "chidori" ]]; then
    # Offline, deterministic agents only: no LLM calls, no user interaction.
    # train.ts carries the interpreter hot paths; the examples cover CLI
    # startup, TS stripping, the journal, and the actor/process runtime.
    for agent in \
        scripts/pgo-train/train.ts \
        examples/agents/hello.ts \
        examples/agents/actor_pipeline.ts; do
        echo "   $agent"
        "$BIN" run "$agent" > /dev/null
    done
else
    # startup.js is the startup-baseline probe, but include it too: real
    # invocations pay realm setup, so its branches deserve profile weight.
    for wl in "$REPO_ROOT/crates/chidori-js/benchmarks/workloads"/*.js; do
        echo "   $(basename "$wl")"
        "$BIN" "$wl" > /dev/null
    done
fi

# An empty profile directory means the corpus silently didn't execute the
# instrumented binary — fail here rather than shipping an unoptimized build
# that claims to be PGO'd.
if ! compgen -G "$PGO_DIR/*.profraw" > /dev/null; then
    echo "no .profraw files were produced by the training corpus" >&2
    exit 1
fi

"$LLVM_PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

echo "== stage 3/3: rebuilding with profile feedback"
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata" \
    cargo build --release -q "${BUILD_ARGS[@]}" "${CARGO_ARGS[@]}" "${TARGET_ARGS[@]}" \
    --target-dir "$TARGET_DIR"

echo "PGO-optimized binary: $BIN"
if [[ -n "$OUT" ]]; then
    cp "$BIN" "$OUT"
    echo "copied to: $OUT"
fi
