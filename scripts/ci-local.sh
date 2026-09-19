#!/usr/bin/env bash
# The same bounded gate runs locally and on each supported CI host.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SDK="$(cd "$ROOT/../../../sdk" && pwd)"
exec python3 "$SDK/scripts/qualify-local.py" --plugin-root "$ROOT" "$@"
