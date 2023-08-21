#!/bin/bash
set -euo pipefail

cd ./chidori
maturin build --release --out dist --features python --target  aarch64-apple-darwin
cd -