#!/bin/bash
set -euo pipefail

cd ./chidori
maturin publish --features python
cd -
