#!/usr/bin/env bash
# Publish chidori and its internal crates to crates.io.
#
# Usage:
#   ./publish.sh --dry-run
#   CARGO_REGISTRY_TOKEN=... ./publish.sh
#   CARGO_REGISTRY_TOKEN=... ./publish.sh --skip-tests
#   CARGO_REGISTRY_TOKEN=... PACKAGES="chidori" ./publish.sh
#
# The default package order is dependency-first:
#   chidori-quickjs-sys -> chidori-quickjs -> chidori

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DRY_RUN=0
SKIP_TESTS=0
ALLOW_DIRTY=0
YES=0
INDEX_POLL_ATTEMPTS="${CRATES_IO_INDEX_POLL_ATTEMPTS:-20}"
INDEX_POLL_SECONDS="${CRATES_IO_INDEX_POLL_SECONDS:-15}"

usage() {
  sed -n '2,11p' "$0"
  cat <<'EOF'

Options:
  --dry-run       Run build, tests, and cargo publish dry-runs only.
  --skip-tests    Skip cargo test.
  --allow-dirty   Allow a real publish from a dirty working tree.
  --yes           Do not prompt when HEAD is not on a v* tag.
  -h, --help      Show this help.
EOF
}

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --skip-tests) SKIP_TESTS=1 ;;
    --allow-dirty) ALLOW_DIRTY=1 ;;
    --yes) YES=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required tool: $1" >&2
    exit 1
  }
}

require cargo
require curl
require git
require python3

# Publish leaves first so each dependent can resolve its internal dependency
# from crates.io when cargo verifies the packaged crate.
PACKAGES="${PACKAGES:-chidori-quickjs-sys chidori-quickjs chidori}"

if [[ "$DRY_RUN" -eq 0 && -z "${CARGO_REGISTRY_TOKEN:-}" ]]; then
  echo "CARGO_REGISTRY_TOKEN is not set. Create one at https://crates.io/me and re-run." >&2
  echo "Use --dry-run to validate without publishing." >&2
  exit 1
fi

if [[ "$DRY_RUN" -eq 0 && "$ALLOW_DIRTY" -eq 0 && -n "$(git status --porcelain)" ]]; then
  echo "working tree has uncommitted changes; commit/stash them or pass --allow-dirty" >&2
  git status --short >&2
  exit 1
fi

if [[ "$DRY_RUN" -eq 0 ]]; then
  CURRENT_TAG="$(git tag --points-at HEAD | grep '^v' | head -n1 || true)"
  if [[ -z "$CURRENT_TAG" && "$YES" -eq 0 ]]; then
    echo "warning: HEAD is not on a v* tag; releases are normally cut from a tag." >&2
    read -r -p "Continue anyway? [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]] || exit 1
  fi
fi

package_version() {
  local pkg="$1"
  cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys
pkg=sys.argv[1]
for package in json.load(sys.stdin)["packages"]:
    if package["name"] == pkg:
        print(package["version"])
        break
else:
    raise SystemExit(f"package not found: {pkg}")' "$pkg"
}

local_package_deps() {
  local pkg="$1"
  cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys
pkg=sys.argv[1]
metadata=json.load(sys.stdin)
workspace_names={package["name"] for package in metadata["packages"]}
for package in metadata["packages"]:
    if package["name"] != pkg:
        continue
    for dep in package["dependencies"]:
        if dep.get("path") and dep["name"] in workspace_names:
            print(dep["name"])
    break
else:
    raise SystemExit(f"package not found: {pkg}")' "$pkg"
}

crate_version_exists() {
  local pkg="$1"
  local version="$2"
  local status
  status="$(curl -A 'chidori-publish-script' -sS -o /dev/null -w '%{http_code}' "https://crates.io/api/v1/crates/${pkg}/${version}")"
  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *)
      echo "could not check crates.io for ${pkg} ${version}; HTTP ${status}" >&2
      exit 1
      ;;
  esac
}

has_unpublished_local_dep() {
  local pkg="$1"
  local dep
  local dep_version

  for dep in $(local_package_deps "$pkg"); do
    dep_version="$(package_version "$dep")"
    if ! crate_version_exists "$dep" "$dep_version"; then
      echo "==> ${pkg} depends on unpublished ${dep} ${dep_version}"
      return 0
    fi
  done

  return 1
}

wait_for_crate_version() {
  local pkg="$1"
  local version="$2"
  local attempt=1

  while [[ "$attempt" -le "$INDEX_POLL_ATTEMPTS" ]]; do
    if crate_version_exists "$pkg" "$version"; then
      echo "==> crates.io sees ${pkg} ${version}"
      return 0
    fi

    echo "==> waiting for crates.io to index ${pkg} ${version} (${attempt}/${INDEX_POLL_ATTEMPTS})"
    sleep "$INDEX_POLL_SECONDS"
    attempt=$((attempt + 1))
  done

  echo "timed out waiting for crates.io to index ${pkg} ${version}" >&2
  exit 1
}

publish_args() {
  if [[ "$ALLOW_DIRTY" -eq 1 || "$DRY_RUN" -eq 1 ]]; then
    printf '%s\n' --allow-dirty
  fi
}

echo "==> packages: $PACKAGES"

echo "==> cargo build --release"
cargo build --release

if [[ "$SKIP_TESTS" -eq 0 ]]; then
  echo "==> cargo test"
  cargo test
fi

published=()
skipped=()

for pkg in $PACKAGES; do
  version="$(package_version "$pkg")"
  echo
  echo "==> checking crates.io: ${pkg} ${version}"
  if crate_version_exists "$pkg" "$version"; then
    echo "==> ${pkg} ${version} is already published; skipping"
    skipped+=("${pkg}@${version}")
    continue
  fi

  if [[ "$DRY_RUN" -eq 1 ]] && has_unpublished_local_dep "$pkg"; then
    echo "==> cargo publish --dry-run cannot verify ${pkg} until its internal dependency is published"
    echo "==> package file-list check: cargo package -p ${pkg} --list"
    cargo package -p "$pkg" --list $(publish_args) >/dev/null
    continue
  fi

  echo "==> dry-run: cargo publish -p ${pkg}"
  cargo publish -p "$pkg" --dry-run $(publish_args)

  if [[ "$DRY_RUN" -eq 1 ]]; then
    continue
  fi

  echo "==> publish: cargo publish -p ${pkg}"
  cargo publish -p "$pkg" --token "$CARGO_REGISTRY_TOKEN" $(publish_args)
  published+=("${pkg}@${version}")

  wait_for_crate_version "$pkg" "$version"
done

echo
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "Dry run complete."
else
  echo "Publish complete."
fi

if [[ "${#published[@]}" -gt 0 ]]; then
  echo "Published: ${published[*]}"
fi
if [[ "${#skipped[@]}" -gt 0 ]]; then
  echo "Already published: ${skipped[*]}"
fi
