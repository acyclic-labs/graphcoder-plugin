#!/usr/bin/env bash
# Shell access to product.toml. Source this, then use $PRODUCT_NAME,
# $PRODUCT_NPM_PACKAGE, $PRODUCT_GITHUB_REPO, $PRODUCT_RELEASE_TAG_PREFIX (or
# call product_key <key>).
PRODUCT_TOML="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/product.toml"
product_key() {
  awk -v k="$1" -F' *= *' '$1 == k { gsub(/^"|"$/, "", $2); print $2; exit }' "$PRODUCT_TOML"
}
PRODUCT_NAME="$(product_key name)"
PRODUCT_NPM_PACKAGE="$(product_key npm_package)"
PRODUCT_GITHUB_REPO="$(product_key github_repo)"
PRODUCT_RELEASE_TAG_PREFIX="$(product_key release_tag_prefix)"
[ -n "$PRODUCT_NAME" ] || { echo "product.toml: no name" >&2; exit 1; }
