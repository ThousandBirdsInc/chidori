#!/bin/bash
set -euo pipefail

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

cd "${GIT_ROOT}/toolchain/chidori"
maturin build --release --out dist --features python --target  aarch64-apple-darwin
cd "${INITIAL_DIR}"