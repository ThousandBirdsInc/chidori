#!/usr/bin/env bash
# Guard against stale npm publishes of the TypeScript SDK.
#
# The release workflow skips publishing any version that is already on npm.
# That makes re-running a tag safe — but it also means that if SDK source
# changes land without a version bump, the npm package silently stays stale
# at the same version number forever. That is exactly how the published
# 3.6.0 ended up missing the 3.6.0 runtime's `defineTool`/`chidori.util`/
# `input details` types (consumer usability review round 4, Finding 2).
#
# This script fails when `sdk/typescript`'s package.json version is already
# published AND the published tarball's contents differ from what `npm pack`
# produces from the tree. The fix is always the same: bump the version train
# (docs/releasing.md).
#
# Usage: ./scripts/check-npm-drift.sh
#   (requires node + npm; runs `npm ci` + `npm run build` in sdk/typescript
#    unless SKIP_BUILD=1 is set and dist/ already exists)

set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/sdk/typescript"

name="$(node -p "require('./package.json').name")"
version="$(node -p "require('./package.json').version")"

tarball_url="$(npm view "${name}@${version}" dist.tarball 2>/dev/null || true)"
if [[ -z "$tarball_url" ]]; then
  echo "ok: ${name}@${version} is not on npm yet — nothing to drift against"
  exit 0
fi

if [[ "${SKIP_BUILD:-0}" != "1" || ! -d dist ]]; then
  npm ci --silent
  npm run build --silent
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

local_tgz="$(npm pack --silent --pack-destination "$workdir" | tail -1)"
curl -fsSL "$tarball_url" -o "$workdir/published.tgz"

mkdir "$workdir/local" "$workdir/published"
tar -xzf "$workdir/$local_tgz" -C "$workdir/local"
tar -xzf "$workdir/published.tgz" -C "$workdir/published"

if diff -r "$workdir/published/package" "$workdir/local/package" > "$workdir/drift.txt" 2>&1; then
  echo "ok: ${name}@${version} on npm matches the tree"
  exit 0
fi

echo "error: ${name}@${version} is already published, but its contents differ" >&2
echo "from what this tree would publish. The release workflow will SKIP this" >&2
echo "version, so the changes below would never reach npm users:" >&2
echo >&2
head -50 "$workdir/drift.txt" >&2
echo >&2
echo "fix: bump the version train (docs/releasing.md) so the change publishes" >&2
exit 1
