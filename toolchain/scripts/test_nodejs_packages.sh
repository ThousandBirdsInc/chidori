#!/bin/bash
set -euo pipefail

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

cd "${GIT_ROOT}/toolchain/chidori"
npm run build
npm run test-js
cd "${INITIAL_DIR}"