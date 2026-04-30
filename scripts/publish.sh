#!/usr/bin/env bash
# Publish chidori to crates.io.
#
# Modeled on the v0.1.x chidori release flow (see .github/workflows/rust.yml in
# pre-v3 history): tag-driven, uses CARGO_REGISTRY_TOKEN, publishes packages in
# dependency order with a dry run first.
#
# Usage:
#   CARGO_REGISTRY_TOKEN=... scripts/publish.sh           # full release
#   scripts/publish.sh --dry-run                          # validate metadata only
#   scripts/publish.sh --skip-tests                       # skip cargo test
#   PACKAGES="chidori" scripts/publish.sh                 # override package list
#
# Pre-release checklist (do this before running):
#   1. Bump `version` in Cargo.toml.
#   2. Update CHANGELOG / release notes.
#   3. Commit, tag `vX.Y.Z`, push tag.
#   4. Run this script (or let CI run it from the tag push).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DRY_RUN=0
SKIP_TESTS=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)    DRY_RUN=1 ;;
    --skip-tests) SKIP_TESTS=1 ;;
    -h|--help)
      sed -n '2,18p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

# Packages to publish, in dependency order (leaves first). Override with the
# PACKAGES env var if you only want to push a subset.
PACKAGES="${PACKAGES:-chidori}"

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }
}
require cargo
require git

if [[ "$DRY_RUN" -eq 0 && -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  echo "CARGO_REGISTRY_TOKEN is not set. Get one at https://crates.io/me and re-run." >&2
  echo "(Use --dry-run to validate without publishing.)" >&2
  exit 1
fi

# Working tree must be clean for a real publish — crates.io forbids it via
# `cargo publish` itself, but we surface the error early with a clearer message.
if [[ "$DRY_RUN" -eq 0 ]]; then
  if [[ -n "$(git status --porcelain)" ]]; then
    echo "working tree has uncommitted changes — commit or stash before publishing" >&2
    git status --short >&2
    exit 1
  fi

  # Encourage tag-driven releases (matches the old chidori workflow trigger).
  CURRENT_TAG="$(git tag --points-at HEAD | grep '^v' | head -n1 || true)"
  if [[ -z "$CURRENT_TAG" ]]; then
    echo "warning: HEAD is not on a v* tag — releases are normally cut from a tag." >&2
    echo "  Create one with:  git tag vX.Y.Z && git push origin vX.Y.Z" >&2
    read -r -p "Continue anyway? [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]] || exit 1
  fi
fi

echo "==> cargo build --release"
cargo build --release

if [[ "$SKIP_TESTS" -eq 0 ]]; then
  echo "==> cargo test"
  cargo test
fi

publish_pkg() {
  local pkg="$1"
  echo
  echo "==> dry-run: cargo publish -p $pkg"
  cargo publish -p "$pkg" --dry-run --allow-dirty=false

  if [[ "$DRY_RUN" -eq 1 ]]; then
    return
  fi

  echo "==> publish: cargo publish -p $pkg"
  cargo publish -p "$pkg" --token "$CARGO_REGISTRY_TOKEN"

  # crates.io needs a moment to index the new version before dependents can
  # find it. Old chidori workflow waited 30s between crates for the same reason.
  echo "==> sleeping 30s so crates.io can index $pkg before the next package"
  sleep 30
}

for pkg in $PACKAGES; do
  publish_pkg "$pkg"
done

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo
  echo "Dry run complete. Re-run without --dry-run (with CARGO_REGISTRY_TOKEN set) to publish."
else
  echo
  echo "Published: $PACKAGES"
fi
