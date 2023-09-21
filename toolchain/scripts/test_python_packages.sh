#!/bin/bash
set -euo pipefail

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

cd "${GIT_ROOT}/toolchain/chidori"
maturin develop --features python
pip install pytest pytest-mock pytest-asyncio
pytest -v ./
cd "${INITIAL_DIR}"
