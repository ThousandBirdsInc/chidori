#!/usr/bin/env bash
# Guard against stale crates.io publishes of the internal `chidori-js` crate.
#
# The crates analogue of check-npm-drift.sh, and the guard this repo was
# missing. `scripts/publish.sh` skips any version already on crates.io, so if
# engine changes land in crates/chidori-js without a version bump, the crate
# silently stays stale at that version forever. That is worse than the npm
# case: when `cargo publish` verifies the packaged `chidori` crate, it
# resolves `chidori-js` from the REGISTRY, not the path dependency — so a
# stale chidori-js fails the release outright with errors about APIs that
# exist in the tree but not in the published crate.
#
# That is exactly how v3.8.0 failed: chidori-js sat at 0.3.3 across a release
# cycle that added RestorePath, DurableBlob::image, Vm::snapshot_image,
# prepare_image_units and more, and the crates job died with 20 E0599/E0609
# errors against chidori-js-0.3.3 while npm and PyPI had already published.
#
# Fails when chidori-js's version is already on crates.io AND the published
# .crate tarball differs from what `cargo package` produces from the tree.
# The fix is always the same: bump crates/chidori-js/Cargo.toml (and the
# matching `version` on chidori's dependency on it).
#
# Usage: ./scripts/check-crate-drift.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CRATE=chidori-js
UA="chidori-release-check (https://github.com/ThousandBirdsInc/chidori)"

version="$(python3 -c 'import tomllib
with open("crates/chidori-js/Cargo.toml", "rb") as f:
    print(tomllib.load(f)["package"]["version"])')"

# crates.io requires a descriptive User-Agent; anonymous requests are refused.
if ! curl -fsSL -H "User-Agent: ${UA}" \
    "https://crates.io/api/v1/crates/${CRATE}/${version}" -o /dev/null 2>/dev/null; then
  echo "ok: ${CRATE} ${version} is not on crates.io yet — nothing to drift against"
  exit 0
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# `cargo package` reproduces exactly what a publish would upload.
cargo package -p "$CRATE" --no-verify --allow-dirty --quiet
local_crate="target/package/${CRATE}-${version}.crate"

curl -fsSL -H "User-Agent: ${UA}" \
  "https://crates.io/api/v1/crates/${CRATE}/${version}/download" -o "$workdir/published.crate"

mkdir "$workdir/local" "$workdir/published"
tar -xzf "$local_crate" -C "$workdir/local"
tar -xzf "$workdir/published.crate" -C "$workdir/published"

# Cargo rewrites these at package time (checksums, path-dep stripping), so
# they differ for reasons unrelated to source drift.
ignore=(-x .cargo_vcs_info.json -x Cargo.toml.orig -x Cargo.lock)

if diff -r "${ignore[@]}" \
     "$workdir/published/${CRATE}-${version}" \
     "$workdir/local/${CRATE}-${version}" > "$workdir/drift.txt" 2>&1; then
  echo "ok: ${CRATE} ${version} on crates.io matches the tree"
  exit 0
fi

echo "error: ${CRATE} ${version} is already on crates.io, but its contents" >&2
echo "differ from what this tree would publish. publish.sh will SKIP it, and" >&2
echo "the chidori publish will then fail to verify against the stale crate:" >&2
echo >&2
head -50 "$workdir/drift.txt" >&2
echo >&2
echo "fix: bump crates/chidori-js/Cargo.toml and chidori's dependency on it" >&2
exit 1
