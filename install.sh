#!/bin/sh
# Install emuhub — a terminal ROM browser for the Miyoo Mini+ / Onion OS.
#
#   curl -fsSL https://raw.githubusercontent.com/glennbarosen/emuhub-tui/main/install.sh | sh
#
# Env:
#   EMUHUB_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   EMUHUB_VERSION       tag to install (default: the latest release)

set -eu

REPO="glennbarosen/emuhub-tui"
INSTALL_DIR="${EMUHUB_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf '\033[31merror:\033[0m %s\n' "$1" >&2
	exit 1
}

info() {
	printf '\033[32m==>\033[0m %s\n' "$1"
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "this script needs \`$1\`, which isn't installed"
}

need tar
if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL "$1" -o "$2"; }
	fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -qO "$2" "$1"; }
	fetch_stdout() { wget -qO- "$1"; }
else
	die "this script needs either \`curl\` or \`wget\`"
fi

# --- Work out which build to fetch -------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "$os/$arch" in
	Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
	Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
	Darwin/arm64) target="aarch64-apple-darwin" ;;
	Darwin/x86_64)
		die "there's no prebuilt Intel macOS binary. Build it yourself:
    cargo install --git https://github.com/$REPO --bin emuhub"
		;;
	*)
		die "unsupported platform: $os $arch. Build it yourself:
    cargo install --git https://github.com/$REPO --bin emuhub"
		;;
esac

if [ -n "${EMUHUB_VERSION:-}" ]; then
	tag="$EMUHUB_VERSION"
else
	info "Looking up the latest release..."
	tag="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" |
		sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
	[ -n "$tag" ] || die "couldn't determine the latest release. Set EMUHUB_VERSION to pick one by hand."
fi

version="${tag#v}"
name="emuhub-${version}-${target}"
url="https://github.com/$REPO/releases/download/${tag}/${name}.tar.gz"

# --- Download and verify ------------------------------------------------------

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading emuhub $tag ($target)"
fetch "$url" "$tmp/$name.tar.gz" || die "download failed: $url"

if fetch "$url.sha256" "$tmp/$name.tar.gz.sha256" 2>/dev/null; then
	info "Verifying checksum"
	if command -v sha256sum >/dev/null 2>&1; then
		checksum_cmd="sha256sum"
	elif command -v shasum >/dev/null 2>&1; then
		checksum_cmd="shasum -a 256"
	else
		checksum_cmd=""
		printf 'warning: no sha256 tool found, skipping verification\n' >&2
	fi

	if [ -n "$checksum_cmd" ]; then
		expected="$(cut -d' ' -f1 <"$tmp/$name.tar.gz.sha256")"
		actual="$($checksum_cmd "$tmp/$name.tar.gz" | cut -d' ' -f1)"
		[ "$expected" = "$actual" ] || die "checksum mismatch — refusing to install
  expected: $expected
  actual:   $actual"
	fi
else
	printf 'warning: no checksum published for this release, skipping verification\n' >&2
fi

# --- Install ------------------------------------------------------------------

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/$name/emuhub" "$INSTALL_DIR/emuhub"

info "Installed emuhub $tag to $INSTALL_DIR/emuhub"

case ":$PATH:" in
	*":$INSTALL_DIR:"*) ;;
	*)
		printf '\n\033[33mnote:\033[0m %s is not on your PATH. Add this to your shell config:\n\n    export PATH="%s:$PATH"\n\n' \
			"$INSTALL_DIR" "$INSTALL_DIR"
		;;
esac

printf 'Run \033[1memuhub <device-ip>\033[0m to get started, or just \033[1memuhub\033[0m and press \033[1ms\033[0m to find your handheld.\n'
