#!/bin/sh
# Chidori installer — downloads a prebuilt `chidori` binary (no Rust toolchain
# required) from the latest GitHub release and puts it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/ThousandBirdsInc/chidori/main/scripts/install.sh | sh
#
# Environment overrides:
#   CHIDORI_VERSION       tag to install (e.g. v3.3.0); default: latest release
#   CHIDORI_INSTALL_DIR   install directory; default: $HOME/.chidori/bin
#
# Prebuilt binaries cover macOS (arm64/x86_64) and Linux (x86_64/arm64). On any
# other platform — or if you'd rather build from source — use `cargo install
# chidori` instead.
set -eu

REPO="ThousandBirdsInc/chidori"
INSTALL_DIR="${CHIDORI_INSTALL_DIR:-$HOME/.chidori/bin}"

err() {
	printf 'error: %s\n' "$1" >&2
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

# Pick whichever HTTP client is present.
fetch() {
	# fetch <url> <dest>
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "$2" "$1"
	else
		err "need curl or wget to download files"
	fi
}

fetch_stdout() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO- "$1"
	else
		err "need curl or wget to download files"
	fi
}

need tar
need uname

# Map uname output to a Rust target triple matching the release asset names.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
Darwin) os_part="apple-darwin" ;;
Linux) os_part="unknown-linux-gnu" ;;
*) err "unsupported OS '$os'. Build from source instead: cargo install chidori" ;;
esac
case "$arch" in
x86_64 | amd64) arch_part="x86_64" ;;
arm64 | aarch64) arch_part="aarch64" ;;
*) err "unsupported architecture '$arch'. Build from source instead: cargo install chidori" ;;
esac
target="${arch_part}-${os_part}"

# Resolve the version to install. The latest-release redirect avoids a JSON
# parser and an authenticated API call.
version="${CHIDORI_VERSION:-}"
if [ -z "$version" ]; then
	version="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" |
		grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
	[ -n "$version" ] || err "could not determine the latest release version; set CHIDORI_VERSION to pin one"
fi

asset="chidori-${version}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${version}/${asset}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'Installing chidori %s (%s)...\n' "$version" "$target"
fetch "$url" "$tmp/$asset" ||
	err "download failed: $url
No prebuilt binary for ${target} at ${version}. Build from source instead: cargo install chidori"

# Verify the checksum when the .sha256 sidecar is published alongside the asset.
if fetch "${url}.sha256" "$tmp/${asset}.sha256" 2>/dev/null; then
	expected="$(cut -d' ' -f1 <"$tmp/${asset}.sha256")"
	if command -v shasum >/dev/null 2>&1; then
		actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
	elif command -v sha256sum >/dev/null 2>&1; then
		actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
	else
		actual=""
	fi
	if [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
		err "checksum mismatch for ${asset} (expected ${expected}, got ${actual})"
	fi
fi

tar -C "$tmp" -xzf "$tmp/$asset"
[ -f "$tmp/chidori" ] || err "archive did not contain a chidori binary"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/chidori" "$INSTALL_DIR/chidori" 2>/dev/null ||
	{ cp "$tmp/chidori" "$INSTALL_DIR/chidori" && chmod 0755 "$INSTALL_DIR/chidori"; }

printf '\nInstalled chidori to %s/chidori\n' "$INSTALL_DIR"

# Nudge the user if the install dir isn't already on PATH.
case ":${PATH}:" in
*":${INSTALL_DIR}:"*) ;;
*)
	printf '\nAdd it to your PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
	printf '(add that line to your ~/.zshrc or ~/.bashrc to make it permanent)\n'
	;;
esac

printf '\nThen check it:\n  chidori --version\n'
