#!/bin/sh
# Installer: downloads the prebuilt binary for this machine from a
# GitHub release, verifies it against the release's SHA256SUMS, and installs
# it into a user-writable bin directory. No sudo, no package manager.
#
#   curl -fsSL https://raw.githubusercontent.com/acyclic-labs/sdk/main/plugin/scripts/install.sh | sh
#
# Environment:
#   ACYCLIC_VERSION      release to install, e.g. 0.0.3 (default: the version
#                        named by plugin/LATEST on the sdk repo's main branch)
#   ACYCLIC_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
#   ACYCLIC_RELEASE_URL  base URL of a release's assets (default: the GitHub
#                        release for ACYCLIC_VERSION); file:// works, which is
#                        how scripts/install-smoke.sh tests this script offline
set -eu

# This script is fetched on its own, so it cannot read product.toml. These
# four lines mirror it; scripts/check-product-name.sh fails CI if they drift.
NAME="acyclic"
REPO="acyclic-labs/sdk"
NPM_PACKAGE="@acyclic-labs/plugin"
TAG_PREFIX="plugin-v"
# The sdk repository hosts several release families, so "latest release" is
# not necessarily this product's. main carries the current version in a file.
LATEST_URL="https://raw.githubusercontent.com/$REPO/main/plugin/LATEST"
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
asset="$NAME-$os-$cpu"

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

if [ -n "${ACYCLIC_RELEASE_URL:-}" ]; then
  base="${ACYCLIC_RELEASE_URL%/}"
else
  version_wanted="${ACYCLIC_VERSION:-}"
  if [ -z "$version_wanted" ]; then
    latest_tmp="$(mktemp "${TMPDIR:-/tmp}/$NAME-latest.XXXXXX")"
    fetch "$LATEST_URL" "$latest_tmp" || die "cannot read $LATEST_URL; set ACYCLIC_VERSION"
    version_wanted="$(tr -d ' \r\n' < "$latest_tmp")"
    rm -f "$latest_tmp"
    [ -n "$version_wanted" ] || die "$LATEST_URL is empty; set ACYCLIC_VERSION"
  fi
  base="https://github.com/$REPO/releases/download/${TAG_PREFIX}${version_wanted#v}"
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/$NAME-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# A failure here is nearly always a missing release rather than a broken
# network: no release cut yet, or ACYCLIC_VERSION naming one that does not
# exist. Say which, and name the install path that does not need a release.
missing() {
  say "install.sh: cannot fetch $1"
  say ""
  say "No release asset at that URL. See which releases exist:"
  say "  https://github.com/$REPO/releases?q=${TAG_PREFIX}"
  say "A release must carry both $asset and SHA256SUMS."
  say ""
  say "To install without a GitHub release:"
  say "  npm i -g $NPM_PACKAGE"
  exit 1
}

say "downloading $asset from $base"
fetch "$base/$asset" "$tmp/$asset" || missing "$base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || missing "$base/SHA256SUMS"

want="$(awk -v a="$asset" '$2 == a || $2 == "*" a || $2 == "./" a {print $1; exit}' "$tmp/SHA256SUMS")"
[ -n "$want" ] || die "SHA256SUMS has no entry for $asset"
got="$(sha256_of "$tmp/$asset")"
[ "$got" = "$want" ] || die "checksum mismatch for $asset: got $got, want $want"

mkdir -p "$INSTALL_DIR"
chmod 0755 "$tmp/$asset"
# Atomic replace so a running daemon keeps its old inode until restart.
mv -f "$tmp/$asset" "$INSTALL_DIR/$NAME"

version="$("$INSTALL_DIR/$NAME" --version 2>/dev/null || true)"
[ -n "$version" ] || die "installed binary does not run on this machine"
say "installed $version to $INSTALL_DIR/$NAME"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "note: $INSTALL_DIR is not on your PATH; add it, e.g."
     say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "next: cd your-repo && $NAME init && $NAME install claude-code"
