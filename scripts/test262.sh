#!/usr/bin/env bash
#
# Vendor the official Test262 ECMAScript conformance suite and run it against
# chidori's pure-Rust JS engine — the same suite Bun and Node measure language
# conformance against.
#
#   scripts/test262.sh                 # vendor (if needed) + run language+built-ins
#   scripts/test262.sh --update        # force re-fetch the pinned checkout
#   scripts/test262.sh --gate          # run the full suite against the committed
#                                      #   baseline; non-zero exit on a regression
#   scripts/test262.sh --update-baseline   # re-record the committed expectations
#   scripts/test262.sh test/language/expressions/addition   # run a subdir
#   scripts/test262.sh --filter Array  # run only paths containing "Array"
#
# Any args after vendoring are forwarded to the runner. See the runner's
# --help for the full flag set.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="${TEST262_DIR:-$REPO_ROOT/vendor/test262}"
BASELINE="${TEST262_BASELINE:-$REPO_ROOT/crates/test262-runner/test262-expectations.json}"
# Pin to a known commit so the conformance number is reproducible. Bump
# deliberately (and refresh the baseline) when tracking newer language
# proposals. GitHub allows fetching an arbitrary commit by SHA.
TEST262_REMOTE="https://github.com/tc39/test262.git"
TEST262_REF="${TEST262_REF:-05bb032907160d66c212589d345fa0e335e2738c}"

update=0
gate=0
update_baseline=0
forward=()
for arg in "$@"; do
  case "$arg" in
    --update) update=1 ;;
    --gate) gate=1 ;;
    --update-baseline) update_baseline=1 ;;
    *) forward+=("$arg") ;;
  esac
done

if [[ ! -d "$VENDOR_DIR/harness" || "$update" == "1" ]]; then
  echo "Vendoring Test262@${TEST262_REF:0:12} into $VENDOR_DIR ..."
  mkdir -p "$VENDOR_DIR"
  if [[ ! -d "$VENDOR_DIR/.git" ]]; then
    git -C "$VENDOR_DIR" init -q
    git -C "$VENDOR_DIR" remote add origin "$TEST262_REMOTE" 2>/dev/null || true
  fi
  git -C "$VENDOR_DIR" fetch --depth 1 origin "$TEST262_REF"
  git -C "$VENDOR_DIR" checkout -q FETCH_HEAD
  echo "Vendored at $(git -C "$VENDOR_DIR" rev-parse --short HEAD)"
fi

echo "Building runner ..."
cargo build --release -p test262-runner
RUNNER="$REPO_ROOT/target/release/test262-runner"

if [[ "$update_baseline" == "1" ]]; then
  echo "Recording baseline -> $BASELINE"
  exec "$RUNNER" --test262 "$VENDOR_DIR" --state "$BASELINE" "${forward[@]}"
fi

if [[ "$gate" == "1" ]]; then
  echo "Gating against baseline -> $BASELINE"
  exec "$RUNNER" --test262 "$VENDOR_DIR" --baseline "$BASELINE" "${forward[@]}"
fi

exec "$RUNNER" --test262 "$VENDOR_DIR" "${forward[@]}"
