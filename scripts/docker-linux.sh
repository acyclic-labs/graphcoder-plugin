#!/usr/bin/env bash
# Run the identical qualification gate on a Linux kernel in a container.
set -euo pipefail
PLUGIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SDK="$(cd "$PLUGIN/../../../sdk" && pwd)"
IMAGE="${1:-rust:1-bookworm}"
FUSE_FLAGS=()
if [ -e /dev/fuse ]; then
  FUSE_FLAGS=(--device /dev/fuse --cap-add SYS_ADMIN)
fi
docker run --rm \
  "${FUSE_FLAGS[@]}" \
  -v "$PLUGIN:/lab/worktrees/graphcoder/plugin-sdk-lab:ro" \
  -v "$SDK:/lab/sdk:ro" \
  -v acyclic-linux-cargo:/cargo \
  -v acyclic-linux-target:/build \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/build/target \
  "$IMAGE" bash -eu -o pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq >/dev/null
    apt-get install -y -qq sqlite3 procps python3 >/dev/null
    python3 /lab/sdk/scripts/qualify-local.py
  '\n