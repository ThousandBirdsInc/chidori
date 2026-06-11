#!/usr/bin/env bash
#
# Targeted Test262 conformance with a PERSISTENT result store.
#
# A full run is slow (~minutes). This wraps the runner's `--state` mode so you
# can re-run just the directories you changed and still see the refreshed
# whole-suite total — the store keeps every other test's last result.
#
#   scripts/conformance.sh full                         # full run, (re)populate the store
#   scripts/conformance.sh test/built-ins/Array         # re-run one dir, refresh total
#   scripts/conformance.sh --filter Promise             # re-run by substring, refresh total
#   scripts/conformance.sh total                        # print the stored total, run nothing
#
# Env:
#   STATE=<path>          (default: conformance-state.json at repo root)
#   TEST262_TIMEOUT_MS    per-test timeout (default 4000)

set -euo pipefail
cd "$(dirname "$0")/.."

STATE="${STATE:-conformance-state.json}"
export TEST262_TIMEOUT_MS="${TEST262_TIMEOUT_MS:-4000}"

cargo build --release -p test262-runner >/dev/null
RUNNER=./target/release/test262-runner

# `total`: recompute/print the stored total without running anything (re-runs an
# empty filter so nothing executes but the merged total is reprinted).
if [[ "${1:-}" == "total" ]]; then
  exec "$RUNNER" --state "$STATE" --filter '\0__none__' test/built-ins/Array
fi

# `full`: drop the path args so the runner walks the default language+built-ins.
if [[ "${1:-}" == "full" ]]; then
  shift
fi

exec "$RUNNER" --state "$STATE" "$@"
