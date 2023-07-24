#!/bin/bash
set -euo pipefail

cd ../prompt-graph-core
cargo build --features build-protos
cd -
