#!/bin/bash
set -euo pipefail

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

cd "${GIT_ROOT}/toolchain/prompt-graph-core"
cargo build --features build-protos
cd "${INITIAL_DIR}"
