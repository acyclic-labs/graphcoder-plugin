#!/usr/bin/env bash
# Assemble one npm platform package around a prebuilt `acyclic` binary.
#
#   packaging/npm/platform-package.sh <os> <cpu> <version> <path-to-binary> <out-dir>
#
# Produces <out-dir>/<npm_package>-<os>-<cpu>/ containing package.json and
# bin/<name>, both from product.toml. The generated launcher lists these as
# optionalDependencies; npm installs only the one whose os/cpu match the host.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../../scripts/product.sh"

os="$1"; cpu="$2"; version="$3"; binary="$4"; out="$5"

case "$os" in darwin|linux|win32) ;; *) echo "unsupported os: $os" >&2; exit 1 ;; esac
case "$cpu" in x64|arm64) ;; *) echo "unsupported cpu: $cpu" >&2; exit 1 ;; esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "bad version: $version" >&2; exit 1; }
[ -f "$binary" ] || { echo "binary not found: $binary" >&2; exit 1; }

# Windows needs the .exe suffix for the shim npm writes to resolve the target.
exe=""
[ "$os" = "win32" ] && exe=".exe"

name="${PRODUCT_NPM_PACKAGE}-${os}-${cpu}"
dir="${out}/${name}"
mkdir -p "${dir}/bin"
install -m 0755 "$binary" "${dir}/bin/${PRODUCT_NAME}${exe}"

cat > "${dir}/package.json" <<JSON
{
  "name": "${name}",
  "version": "${version}",
  "description": "${PRODUCT_NAME} prebuilt binary for ${os} ${cpu}. Install ${PRODUCT_NPM_PACKAGE} instead of this package.",
  "license": "Apache-2.0",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/${PRODUCT_GITHUB_REPO}.git"
  },
  "os": ["${os}"],
  "cpu": ["${cpu}"],
  "bin": { "${PRODUCT_NAME}": "bin/${PRODUCT_NAME}${exe}" },
  "files": ["bin/"],
  "publishConfig": { "access": "public", "provenance": true }
}
JSON

echo "$dir"
