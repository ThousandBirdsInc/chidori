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
with open("crates/chidori/Cargo.toml", "rb") as f:
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

# The internal chidori-js crate is NOT on the train — it carries its own 0.x
# version — but chidori's dependency on it must pin that exact version, since
# `cargo publish` verifies the packaged chidori against the REGISTRY copy.
js_version() {
  python3 -c 'import tomllib
with open("crates/chidori-js/Cargo.toml", "rb") as f:
    print(tomllib.load(f)["package"]["version"])'
}

# Every workspace crate that pins a version on chidori-js, as "path=version"
# lines. chidori-wasm pins it too, and a pin missed there fails the same way.
js_dep_pins() {
  python3 -c 'import glob, tomllib
for path in sorted(glob.glob("crates/*/Cargo.toml")):
    with open(path, "rb") as f:
        dep = tomllib.load(f).get("dependencies", {}).get("chidori-js")
    if isinstance(dep, dict) and "version" in dep:
        print(path + "=" + dep["version"])'
}

cd "$REPO_ROOT"

CRATE="$(crate_version)"
TS="$(ts_version)"
PY="$(py_version)"
JS="$(js_version)"
JS_PINS="$(js_dep_pins)"

echo "chidori crate:           ${CRATE}"
echo "TypeScript SDK:          ${TS}"
echo "Python SDK:               ${PY}"
echo "chidori-js crate:        ${JS}"
while IFS='=' read -r pin_path pin_ver; do
  [[ -z "$pin_path" ]] && continue
  echo "  pinned by ${pin_path}: ${pin_ver}"
done <<< "$JS_PINS"
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
while IFS='=' read -r pin_path pin_ver; do
  [[ -z "$pin_path" ]] && continue
  if [[ "$pin_ver" != "$JS" ]]; then
    echo "error: ${pin_path} pins chidori-js ${pin_ver} but the crate is ${JS};" >&2
    echo "       cargo verifies each packaged crate against the REGISTRY copy" >&2
    echo "       of chidori-js, not the path dependency, so these must match." >&2
    echo "       Bump the pin together with crates/chidori-js/Cargo.toml." >&2
    status=1
  fi
done <<< "$JS_PINS"

if [[ "$status" -eq 0 ]]; then
  echo "ok: all release versions agree"
fi
exit "$status"
