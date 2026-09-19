#!/usr/bin/env bash
# CI guard for the single-source product name (product.toml):
#   1. scripts/install.sh is fetched standalone and mirrors `name`,
#      `github_repo`, `npm_package`, and `release_tag_prefix`; they must
#      match exactly.
#   2. No user-facing Rust source spells the name out. Only crate/module
#      identifiers (acyclic_fs, acyclic_engine, acyclic-fs ...) and comments
#      may contain it; strings, paths, and doc templates go through
#      `product::NAME` / `product::render`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/scripts/product.sh"
fail=0

want_name="$PRODUCT_NAME"; want_repo="$PRODUCT_GITHUB_REPO"; want_npm="$PRODUCT_NPM_PACKAGE"
want_prefix="$PRODUCT_RELEASE_TAG_PREFIX"
have_name="$(awk -F'"' '/^NAME=/{print $2; exit}' "$ROOT/scripts/install.sh")"
have_repo="$(awk -F'"' '/^REPO=/{print $2; exit}' "$ROOT/scripts/install.sh")"
have_npm="$(awk -F'"' '/^NPM_PACKAGE=/{print $2; exit}' "$ROOT/scripts/install.sh")"
have_prefix="$(awk -F'"' '/^TAG_PREFIX=/{print $2; exit}' "$ROOT/scripts/install.sh")"
[ "$have_name" = "$want_name" ] || { echo "scripts/install.sh NAME=$have_name, product.toml name=$want_name" >&2; fail=1; }
[ "$have_repo" = "$want_repo" ] || { echo "scripts/install.sh REPO=$have_repo, product.toml github_repo=$want_repo" >&2; fail=1; }
[ "$have_npm" = "$want_npm" ] || { echo "scripts/install.sh NPM_PACKAGE=$have_npm, product.toml npm_package=$want_npm" >&2; fail=1; }
[ "$have_prefix" = "$want_prefix" ] || { echo "scripts/install.sh TAG_PREFIX=$have_prefix, product.toml release_tag_prefix=$want_prefix" >&2; fail=1; }

# The PyPI package is a second mirror: its pyproject.toml carries the name
# and cannot read product.toml at build time.
want_pypi="$(product_key pypi_package)"
have_pypi="$(awk -F'"' '/^name = /{print $2; exit}' "$ROOT/packaging/pypi/pyproject.toml")"
[ "$have_pypi" = "$want_pypi" ] || { echo "packaging/pypi/pyproject.toml name=$have_pypi, product.toml pypi_package=$want_pypi" >&2; fail=1; }

# Literal uses of the current name in strings/paths of user-facing crates.
# Comments (// and //!) are allowed; identifiers with '_' are crate paths.
stray="$(grep -rn --include='*.rs' -E "\"[^\"]*\b${want_name}\b[^\"]*\"|\`${want_name} |\.${want_name}/" \
  "$ROOT/crates/acyclic/src" "$ROOT/crates/acyclic-engine/src" \
  | grep -v -E "${want_name}[_-](fs|engine|proto|qual)|^[^:]+:[0-9]+:\s*//" || true)"
if [ -n "$stray" ]; then
  echo "hardcoded product name in Rust source (use product::NAME / product::render):" >&2
  echo "$stray" >&2
  fail=1
fi
[ "$fail" -eq 0 ] && echo "product name '$want_name' is consistent"
exit "$fail"
