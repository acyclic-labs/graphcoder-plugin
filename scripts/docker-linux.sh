#!/usr/bin/env bash
# Linux validation without leaving the Mac: build and run the full test +
# acceptance suite inside a Linux container. Sources are mounted read-only
# in the sibling layout the path deps expect; all build artifacts and test
# state stay on container-local filesystems (which is also what exercises
# renameat2(RENAME_EXCHANGE) and inotify on a Linux kernel for real).
#
# Usage: scripts/docker-linux.sh [image]
set -euo pipefail

PLUGIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FS="$(cd "$PLUGIN/../fs" && pwd)"
IMAGE="${1:-rust:1-bookworm}"

docker run --rm \
  -v "$FS:/src/fs:ro" \
  -v "$PLUGIN:/src/graphcoder-plugin:ro" \
  -v acyclic-linux-cargo:/cargo \
  -v acyclic-linux-target:/build \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/build/target \
  "$IMAGE" bash -eu -o pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq sqlite3 procps python3 >/dev/null

    cd /src/graphcoder-plugin
    echo "=== cargo test (workspace)"
    cargo test --workspace 2>&1 | grep -E "test result|error" || true
    cargo test --workspace >/dev/null

    echo "=== release build"
    cargo build --release

    echo "=== acceptance suite"
    export TMPDIR=/tmp
    export ACYCLIC_BIN=/build/target/release/acyclic
    export ACYCLIC_QUAL=/build/target/release/acyclic-qual
    export ACYCLIC_LAT_FILES=5000
    export ACYCLIC_LAT_MB=64
    export ACYCLIC_SOAK_ROUNDS=30
    bash tests/acceptance/run-all.sh
  '
