#!/usr/bin/env bash
# Linux validation without leaving the Mac: build and run the full test +
# acceptance suite inside a Linux container. All build artifacts and test
# state stay on container-local filesystems (which is also what exercises
# renameat2(RENAME_EXCHANGE) and inotify on a Linux kernel for real).
#
# Usage: scripts/docker-linux.sh [image]
set -euo pipefail

PLUGIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${1:-rust:1-bookworm}"
# Mount the cargo workspace root: this tree when it is standalone, its parent
# when it is the sdk's `plugin/` member.
if grep -q 'plugin/crates' "$PLUGIN/../Cargo.toml" 2>/dev/null; then
  SRC="$(cd "$PLUGIN/.." && pwd)"; WORKDIR=/src/plugin
else
  SRC="$PLUGIN"; WORKDIR=/src
fi

# FUSE inside the container (fork mounts): pass the device + cap when the
# host offers them; forks.sh skips gracefully otherwise.
FUSE_FLAGS=()
if [ -e /dev/fuse ] || [ "$(uname -s)" = "Darwin" ]; then
  FUSE_FLAGS=(--device /dev/fuse --cap-add SYS_ADMIN)
fi

docker run --rm \
  "${FUSE_FLAGS[@]}" \
  -v "$SRC:/src:ro" \
  -v acyclic-linux-cargo:/cargo \
  -v acyclic-linux-target:/build \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/build/target \
  -e CARGO_NET_GIT_FETCH_WITH_CLI=true \
  -e WORKDIR="$WORKDIR" \
  "$IMAGE" bash -eu -o pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq sqlite3 procps python3 >/dev/null

    cd "$WORKDIR"
    CRATES="-p acyclic -p acyclic-engine -p acyclic-proto -p acyclic-qual"
    echo "=== cargo test (plugin crates)"
    cargo test $CRATES 2>&1 | grep -E "test result|error" || true
    cargo test $CRATES >/dev/null

    echo "=== release build"
    cargo build --release $CRATES

    echo "=== acceptance suite"
    export TMPDIR=/tmp
    export ACYCLIC_BIN=/build/target/release/acyclic
    export ACYCLIC_QUAL=/build/target/release/acyclic-qual
    export ACYCLIC_LAT_FILES=5000
    export ACYCLIC_LAT_MB=64
    export ACYCLIC_SOAK_ROUNDS=30
    bash tests/acceptance/run-all.sh
  '
