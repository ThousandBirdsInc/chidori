#!/usr/bin/env bash
# Verify the release version train: the root crate, the TypeScript SDK, and
# the Python SDK must all carry the same version. Releases are cut from a
# single vX.Y.Z tag, so the workflow also passes the tag version as an
# expected value.
#
# Usage:
#   ./scripts/check-sdk-versions.sh          # packages must agree with each other
#   ./scripts/check-sdk-versions.sh 3.0.0    # ...and with the expected version

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED="${1:-}"

crate_version() {
  python3 -c 'import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["package"]["version"])'
}

ts_version() {
  python3 -c 'import json
with open("sdk/typescript/package.json") as f:
    print(json.load(f)["version"])'
}

py_version() {
  python3 -c 'import tomllib
with open("sdk/python/pyproject.toml", "rb") as f:
    print(tomllib.load(f)["project"]["version"])'
}

cd "$REPO_ROOT"

CRATE="$(crate_version)"
TS="$(ts_version)"
PY="$(py_version)"

echo "chidori crate:           ${CRATE}"
echo "TypeScript SDK:          ${TS}"
echo "Python SDK:               ${PY}"
[[ -n "$EXPECTED" ]] && echo "expected (release tag):   ${EXPECTED}"

status=0
if [[ "$TS" != "$CRATE" || "$PY" != "$CRATE" ]]; then
  echo "error: crate and SDK versions disagree; bump them together" >&2
  status=1
fi
if [[ -n "$EXPECTED" && "$CRATE" != "$EXPECTED" ]]; then
  echo "error: versions do not match the release tag ${EXPECTED}" >&2
  status=1
fi

if [[ "$status" -eq 0 ]]; then
  echo "ok: all release versions agree"
fi
exit "$status"
