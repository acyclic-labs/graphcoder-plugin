#!/bin/sh
# acyclic installer: downloads the prebuilt binary for this machine from a
# GitHub release, verifies it against the release's SHA256SUMS, and installs
# it into a user-writable bin directory. No sudo, no package manager.
#
#   curl -fsSL https://raw.githubusercontent.com/acyclic-labs/graphcoder-plugin/main/scripts/install.sh | sh
#
# Environment:
#   ACYCLIC_VERSION      release to install, e.g. 0.0.2 (default: latest)
#   ACYCLIC_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
#   ACYCLIC_RELEASE_URL  base URL of a release's assets (default: the GitHub
#                        release for ACYCLIC_VERSION); file:// works, which is
#                        how scripts/install-smoke.sh tests this script offline
set -eu

REPO="acyclic-labs/graphcoder-plugin"
INSTALL_DIR="${ACYCLIC_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
die() { say "install.sh: $*"; exit 1; }

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) die "unsupported OS $(uname -s); see https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) cpu=arm64 ;;
  x86_64|amd64) cpu=x64 ;;
  *) die "unsupported CPU $(uname -m); see https://github.com/$REPO/releases" ;;
esac
asset="acyclic-$os-$cpu"

if [ -n "${ACYCLIC_RELEASE_URL:-}" ]; then
  base="${ACYCLIC_RELEASE_URL%/}"
elif [ -n "${ACYCLIC_VERSION:-}" ]; then
  base="https://github.com/$REPO/releases/download/v${ACYCLIC_VERSION#v}"
else
  base="https://github.com/$REPO/releases/latest/download"
fi

fetch() {
  # fetch <url> <dest>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/acyclic-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset from $base"
fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

want="$(awk -v a="$asset" '$2 == a || $2 == "*" a || $2 == "./" a {print $1; exit}' "$tmp/SHA256SUMS")"
[ -n "$want" ] || die "SHA256SUMS has no entry for $asset"
got="$(sha256_of "$tmp/$asset")"
[ "$got" = "$want" ] || die "checksum mismatch for $asset: got $got, want $want"

mkdir -p "$INSTALL_DIR"
chmod 0755 "$tmp/$asset"
# Atomic replace so a running daemon keeps its old inode until restart.
mv -f "$tmp/$asset" "$INSTALL_DIR/acyclic"

version="$("$INSTALL_DIR/acyclic" --version 2>/dev/null || true)"
[ -n "$version" ] || die "installed binary does not run on this machine"
say "installed $version to $INSTALL_DIR/acyclic"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "note: $INSTALL_DIR is not on your PATH; add it, e.g."
     say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "next: cd your-repo && acyclic init && acyclic install claude-code"
