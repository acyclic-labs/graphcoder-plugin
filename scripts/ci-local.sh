#!/usr/bin/env bash
# Runs what .github/workflows/ci.yml runs, job by job, on this machine, and
# prints one line per step at the end so a red step is easy to find. Every
# step runs even after an earlier one fails (CI's jobs are independent too);
# the exit code is non-zero if any step failed.
#
#   scripts/ci-local.sh                  everything, including the acceptance suite (slowest)
#   scripts/ci-local.sh --no-acceptance  the static gates, unit tests, release build, coverage
#   scripts/ci-local.sh --no-coverage    skip cargo-llvm-cov (needs `cargo install cargo-llvm-cov`)
#
# Needs: stable toolchain with rustfmt + clippy, cargo-deny, node (for the
# duplication check), and unless --no-coverage: cargo-llvm-cov plus the
# `llvm-tools-preview` rustup component it drives
# (`rustup component add llvm-tools-preview`).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# The plugin's own crates, whether this tree is a standalone workspace or the
# sdk's `plugin/` member; `--workspace` would lint and test the whole sdk.
CRATES=(-p acyclic -p acyclic-engine -p acyclic-proto -p acyclic-qual)
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
[ -d "$TARGET_DIR" ] || TARGET_DIR="$ROOT/../target"
# Same as CI: the pinned acyclic-fs git dependency needs the git CLI's
# credentials and protocol support, not cargo's built-in fetcher.
export CARGO_NET_GIT_FETCH_WITH_CLI=true

run_acceptance=1
run_coverage=1
for arg in "$@"; do
  case "$arg" in
    --no-acceptance) run_acceptance=0 ;;
    --no-coverage) run_coverage=0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

results=()
failed=0
step() {
  local name="$1"
  shift
  echo
  echo "=== $name"
  local started=$SECONDS
  if "$@"; then
    results+=("PASS  $((SECONDS - started))s  $name")
  else
    results+=("FAIL  $((SECONDS - started))s  $name")
    failed=1
  fi
}

# deny job
step "product name single-sourced" bash scripts/check-product-name.sh
step "code quality (width, TODOs, comment blocks, duplication)" bash scripts/check-code-quality.sh
step "cargo deny" cargo deny --locked check licenses

# lint job
step "cargo fmt --check" cargo fmt "${CRATES[@]}" --check
step "cargo clippy -D warnings" cargo clippy "${CRATES[@]}" --all-targets --all-features -- -D warnings

# test job
step "cargo test" cargo test "${CRATES[@]}"
step "cargo build --release" cargo build --release "${CRATES[@]}"
if [ "$run_acceptance" -eq 1 ]; then
  step "acceptance suite" env \
    ACYCLIC_BIN="$TARGET_DIR/release/acyclic" \
    ACYCLIC_QUAL="$TARGET_DIR/release/acyclic-qual" \
    ACYCLIC_LAT_FILES=5000 ACYCLIC_LAT_MB=64 ACYCLIC_SOAK_ROUNDS=30 \
    bash tests/acceptance/run-all.sh
fi

# coverage job
if [ "$run_coverage" -eq 1 ]; then
  # Informational floor for the plugin crates alone; the sdk gate measures
  # the whole workspace.
  step "coverage (plugin crates, floor 48)" cargo llvm-cov "${CRATES[@]}" --all-features --summary-only --fail-under-lines 48
fi

echo
echo "=== summary"
printf '%s\n' "${results[@]}"
exit "$failed"
