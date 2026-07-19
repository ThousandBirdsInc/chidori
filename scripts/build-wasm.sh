#!/usr/bin/env bash
# Build the browser artifact for crates/chidori-wasm.
#
# Produces crates/chidori-wasm/www/pkg/ (ES module + .wasm), which
# crates/chidori-wasm/www/index.html loads. Serve that directory over HTTP
# (wasm modules cannot be loaded from file://):
#
#   scripts/build-wasm.sh
#   python3 -m http.server -d crates/chidori-wasm/www 8080
#   open http://localhost:8080
#
# Requires: `rustup target add wasm32-unknown-unknown` and a wasm-bindgen-cli
# whose version matches the wasm-bindgen dependency pinned in
# crates/chidori-wasm/Cargo.toml (`cargo install wasm-bindgen-cli`).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -p chidori-wasm --release --target wasm32-unknown-unknown
wasm-bindgen --target web \
  --out-dir crates/chidori-wasm/www/pkg \
  target/wasm32-unknown-unknown/release/chidori_wasm.wasm

# The browser SDK ships from sdk/browser; the demo page imports it alongside
# the wasm bindings, so mirror it into the (gitignored) pkg/ output.
cp sdk/browser/index.js crates/chidori-wasm/www/pkg/chidori-browser.js

# Mirror the runtime assets into the docs website's public dir (also
# gitignored) so the /playground page can load them. The docs deploy workflow
# runs this script before `next build`.
mkdir -p website/public/chidori-wasm
cp crates/chidori-wasm/www/pkg/chidori_wasm.js \
   crates/chidori-wasm/www/pkg/chidori_wasm_bg.wasm \
   crates/chidori-wasm/www/pkg/chidori-browser.js \
   crates/chidori-wasm/www/data/fact.json \
   website/public/chidori-wasm/

echo "Built crates/chidori-wasm/www/pkg:"
ls -la crates/chidori-wasm/www/pkg
