#!/bin/sh
# The installer moved with the plugin into acyclic-labs/sdk. This shim keeps
# the old one-liner working by running the current installer from there.
#
# These four lines mirror product.toml so scripts/check-product-name.sh still
# holds; the real installer at the URL below is what consults them.
NAME="acyclic"
REPO="acyclic-labs/sdk"
NPM_PACKAGE="@acyclic-labs/plugin"
TAG_PREFIX="plugin-v"
set -eu
url="https://raw.githubusercontent.com/$REPO/main/plugin/scripts/install.sh"
if command -v curl >/dev/null 2>&1; then
  script="$(curl -fsSL "$url")"
elif command -v wget >/dev/null 2>&1; then
  script="$(wget -qO- "$url")"
else
  echo "install.sh: need curl or wget" >&2
  exit 1
fi
exec sh -c "$script"
