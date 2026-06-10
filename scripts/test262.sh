#!/usr/bin/env bash
#
# Vendor the official Test262 ECMAScript conformance suite and run it against
# chidori's embedded QuickJS runtime — the same suite Bun and Node measure
# language conformance against.
#
#   scripts/test262.sh                 # vendor (if needed) + run language+built-ins
#   scripts/test262.sh --update        # force re-pin/refresh the checkout
#   scripts/test262.sh test/language/expressions/addition   # run a subdir
#   scripts/test262.sh --filter Array  # run only paths containing "Array"
#
# Any args after vendoring are forwarded to the runner. See the runner's
# --help for the full flag set.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="${TEST262_DIR:-$REPO_ROOT/vendor/test262}"
# Pin to a known commit so the conformance number is reproducible. Bump
# deliberately when you want to track newer language proposals.
TEST262_REMOTE="https://github.com/tc39/test262.git"
TEST262_REF="${TEST262_REF:-main}"

update=0
forward=()
for arg in "$@"; do
  case "$arg" in
    --update) update=1 ;;
    *) forward+=("$arg") ;;
  esac
done

if [[ ! -d "$VENDOR_DIR/harness" || "$update" == "1" ]]; then
  echo "Vendoring Test262 into $VENDOR_DIR ..."
  mkdir -p "$(dirname "$VENDOR_DIR")"
  if [[ -d "$VENDOR_DIR/.git" ]]; then
    git -C "$VENDOR_DIR" fetch --depth 1 origin "$TEST262_REF"
    git -C "$VENDOR_DIR" checkout -q FETCH_HEAD
  else
    git clone --depth 1 --branch "$TEST262_REF" "$TEST262_REMOTE" "$VENDOR_DIR"
  fi
  echo "Vendored at $(git -C "$VENDOR_DIR" rev-parse --short HEAD)"
fi

echo "Building runner ..."
cargo build --release -p test262-runner

exec "$REPO_ROOT/target/release/test262-runner" --test262 "$VENDOR_DIR" "${forward[@]}"
