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
#   scripts/test262.sh --report        # full run -> Markdown coverage table
#                                      #   (test262-coverage.md; CI posts it on PRs)
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
report=0
forward=()
for arg in "$@"; do
  case "$arg" in
    --update) update=1 ;;
    --gate) gate=1 ;;
    --update-baseline) update_baseline=1 ;;
    --report) report=1 ;;
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

# The runner now fans the file loop out across cores (one worker per CPU by
# default; override with TEST262_JOBS). Pin the per-test timeout the *committed
# baseline was recorded with* so the gate stays reproducible no matter what the
# runner's compiled-in default is — otherwise a slow-but-passing test could flip
# to a timeout failure and read as a phantom regression. Refresh the baseline
# (and this pin) deliberately if the budget ever changes.
export TEST262_TIMEOUT_MS="${TEST262_TIMEOUT_MS:-10000}"

# The runner's reference-counting GC cannot reclaim every object cycle, so a
# single process that walks all ~47k tests grows until it is OOM-killed. We
# therefore run the suite one second-level directory at a time, in a fresh
# process each, so memory is reclaimed between chunks. The --state file merges
# results across chunks; --baseline gates each chunk against the full baseline.
#
# Optional CI sharding: when TEST262_SHARD_TOTAL > 1, this process is one of N
# shards (0-based TEST262_SHARD_INDEX) and runs only its slice of the dirs,
# assigned round-robin so the heavy dirs (language/expressions, language/
# statements, built-ins/Array, built-ins/TypedArray, …) are spread across shards
# rather than piling onto one. Each shard still gates its slice against the FULL
# committed baseline, so every test is owned by exactly one shard and the union
# is complete. Default (total=1) runs everything. Sharding applies only to the
# default dir sweep, not to explicit path arguments.
chunk_dirs() {
  if [[ ${#forward[@]} -gt 0 ]]; then
    printf '%s\n' "${forward[@]}"
  else
    local total="${TEST262_SHARD_TOTAL:-1}" index="${TEST262_SHARD_INDEX:-0}"
    (cd "$VENDOR_DIR" && ls -d test/language/*/ test/built-ins/*/) \
      | awk -v t="$total" -v i="$index" 't <= 1 || ((NR - 1) % t) == i'
  fi
}

if [[ "$update_baseline" == "1" ]]; then
  echo "Recording baseline (chunked) -> $BASELINE"
  rm -f "$BASELINE"
  while IFS= read -r d; do
    echo "  $d"
    # The runner exits non-zero when any test in the chunk fails — expected
    # while recording (the baseline records those failures); don't let
    # `set -e` abort the sweep.
    "$RUNNER" --test262 "$VENDOR_DIR" --state "$BASELINE" "$d" >/dev/null || true
  done < <(chunk_dirs)
  echo "Baseline recorded -> $BASELINE"
  exec "$RUNNER" --test262 "$VENDOR_DIR" --state "$BASELINE" --max 0
fi

if [[ "$gate" == "1" ]]; then
  echo "Gating against baseline (chunked) -> $BASELINE"
  current="$(mktemp)"
  status=0
  while IFS= read -r d; do
    "$RUNNER" --test262 "$VENDOR_DIR" --state "$current" --baseline "$BASELINE" "$d" || status=1
  done < <(chunk_dirs)
  echo
  echo "Aggregated current results:"
  "$RUNNER" --test262 "$VENDOR_DIR" --state "$current" --max 0
  [[ "$status" -eq 0 ]] && echo "PASS: no conformance regressions." \
    || echo "FAIL: conformance regression(s) above."
  exit "$status"
fi

if [[ "$report" == "1" ]]; then
  # Run the whole suite (chunked, never sharded) into a fresh state file, then
  # render a Markdown coverage table. Unlike --gate this never fails on
  # regressions — it is a reporting pass (CI posts it as a PR comment).
  REPORT_OUT="${TEST262_REPORT_OUT:-$REPO_ROOT/test262-coverage.md}"
  state="$(mktemp)"
  echo "Measuring coverage (chunked) ..." >&2
  while IFS= read -r d; do
    TEST262_SHARD_TOTAL=1 "$RUNNER" --test262 "$VENDOR_DIR" --state "$state" "$d" >/dev/null || true
  done < <(TEST262_SHARD_TOTAL=1 chunk_dirs)
  python3 "$REPO_ROOT/scripts/test262-coverage.py" "$state" >"$REPORT_OUT"
  echo "Coverage report -> $REPORT_OUT" >&2
  cat "$REPORT_OUT"
  exit 0
fi

exec "$RUNNER" --test262 "$VENDOR_DIR" "${forward[@]}"

