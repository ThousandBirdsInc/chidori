#!/bin/bash
set -euo pipefail

# For the majority of platforms this is automated via our GitHub actions.
# However in the case of macosx_arm64 we need to build locally and then
# publish the package to pypi.

INITIAL_DIR=$(pwd)
GIT_ROOT=$(git rev-parse --show-toplevel)

cd "${GIT_ROOT}/toolchain/chidori"
maturin publish --features python
cd "${INITIAL_DIR}"
