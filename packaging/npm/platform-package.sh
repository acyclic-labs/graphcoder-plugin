#!/usr/bin/env bash
# Assemble one npm platform package around a prebuilt `acyclic` binary.
#
#   packaging/npm/platform-package.sh <os> <cpu> <version> <path-to-binary> <out-dir>
#
# Produces <out-dir>/@acyclic-labs/plugin-<os>-<cpu>/ containing package.json
# and bin/acyclic. The launcher package (packaging/npm/acyclic) lists these as
# optionalDependencies; npm installs only the one whose os/cpu match the host.
set -euo pipefail

os="$1"; cpu="$2"; version="$3"; binary="$4"; out="$5"

case "$os" in darwin|linux) ;; *) echo "unsupported os: $os" >&2; exit 1 ;; esac
case "$cpu" in x64|arm64) ;; *) echo "unsupported cpu: $cpu" >&2; exit 1 ;; esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "bad version: $version" >&2; exit 1; }
[ -f "$binary" ] || { echo "binary not found: $binary" >&2; exit 1; }

name="@acyclic-labs/plugin-${os}-${cpu}"
dir="${out}/${name}"
mkdir -p "${dir}/bin"
install -m 0755 "$binary" "${dir}/bin/acyclic"

cat > "${dir}/package.json" <<JSON
{
  "name": "${name}",
  "version": "${version}",
  "description": "acyclic prebuilt binary for ${os} ${cpu}. Install @acyclic-labs/plugin instead of this package.",
  "license": "Apache-2.0",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/acyclic-labs/graphcoder-plugin.git"
  },
  "os": ["${os}"],
  "cpu": ["${cpu}"],
  "bin": { "acyclic": "bin/acyclic" },
  "files": ["bin/"],
  "publishConfig": { "access": "public", "provenance": true }
}
JSON

echo "$dir"
