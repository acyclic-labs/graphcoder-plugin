#!/usr/bin/env bash
# Clean-machine install flow, the Phase 4 exit gate: on a bare Debian
# container with nothing but curl, run scripts/install.sh against a local
# release directory (same layout the GitHub release has: the binaries plus
# SHA256SUMS), then init a repo, checkpoint, edit, and rewind.
#
#   scripts/install-smoke.sh [release-dir]   (default: dist/bin)
#
# Needs docker and a linux-x64 binary in the release dir. Builds nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${1:-$ROOT/dist/bin}"
command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; }

# Prefer the container arch that runs natively on this host; fall back to
# x64 under emulation. Either way the binary must exist in the release dir.
case "$(uname -m)" in
  arm64|aarch64) PREFER=arm64; OTHER=x64 ;;
  *) PREFER=x64; OTHER=arm64 ;;
esac
if [ -f "$SRC/acyclic-linux-$PREFER" ]; then CPU=$PREFER
elif [ -f "$SRC/acyclic-linux-$OTHER" ]; then CPU=$OTHER
else echo "no linux binary in $SRC (run packaging/npm/release-local.sh build --all)" >&2; exit 1; fi
case "$CPU" in arm64) PLATFORM=linux/arm64 ;; *) PLATFORM=linux/amd64 ;; esac

# Assemble a release directory exactly as .github/workflows/release.yml does.
REL="$(mktemp -d "${TMPDIR:-/tmp}/acyclic-release.XXXXXX")"
trap 'rm -rf "$REL"' EXIT
cp "$SRC/acyclic-linux-$CPU" "$REL/"
(cd "$REL" && shasum -a 256 "acyclic-linux-$CPU" > SHA256SUMS)
echo "release dir: $REL ($CPU, $PLATFORM)"

docker run --rm --platform "$PLATFORM" \
  -v "$ROOT/scripts/install.sh:/install.sh:ro" \
  -v "$REL:/release:ro" \
  debian:bookworm-slim bash -eu -o pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null && apt-get install -y -qq curl ca-certificates >/dev/null
    useradd -m dev
    su dev -c "
      set -eu
      export HOME=/home/dev
      cd \$HOME
      ACYCLIC_RELEASE_URL=file:///release sh /install.sh
      export PATH=\$HOME/.local/bin:\$PATH
      acyclic --version
      mkdir demo && cd demo
      printf ORIGINAL > app.txt
      printf SECRET > .env
      mkdir .acyclic && printf \"exclude = [\\\".env\\\"]\\n\" > .acyclic/config.toml
      acyclic init
      acyclic install agents-md >/dev/null
      acyclic checkpoint --wait -m start
      printf CHANGED > app.txt
      printf LEAKED > .env
      sleep 0.5
      acyclic checkpoint --wait -m edit
      acyclic rewind --last --yes >/dev/null
      cd \"\$PWD\"   # a rewind swaps the directory inode; re-enter it
      acyclic timeline | head -5
      acyclic rewind 1 --yes
      cd \"\$PWD\"
      [ \"\$(cat app.txt)\" = ORIGINAL ] || { echo rewind failed; exit 1; }
      [ \"\$(cat .env)\" = LEAKED ] || { echo excluded file was not carried; exit 1; }
      acyclic status
      acyclic stop
      echo CLEAN-MACHINE-OK
    "
  '
