#!/usr/bin/env bash
# Generate the npm launcher package from product.toml and the workspace
# version. Nothing about it is checked in: the package name, bin name, and
# platform-package names all derive from product.toml.
#
#   packaging/npm/launcher-package.sh <version> <out-dir>
#
# Produces <out-dir>/<npm_package>/{package.json,bin/<name>.js}.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../../scripts/product.sh"

version="$1"; out="$2"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "bad version: $version" >&2; exit 1; }

name="$PRODUCT_NAME"; pkg="$PRODUCT_NPM_PACKAGE"; repo="$PRODUCT_GITHUB_REPO"
dir="${out}/${pkg}"
mkdir -p "${dir}/bin"

cat > "${dir}/package.json" <<JSON
{
  "name": "${pkg}",
  "version": "${version}",
  "description": "Agent-native state engine: checkpoints, rewind, and blast-radius diff for coding-agent sessions",
  "license": "Apache-2.0",
  "repository": { "type": "git", "url": "git+https://github.com/${repo}.git" },
  "bin": { "${name}": "bin/${name}.js" },
  "files": ["bin/"],
  "engines": { "node": ">=18" },
  "optionalDependencies": {
    "${pkg}-darwin-arm64": "${version}",
    "${pkg}-darwin-x64": "${version}",
    "${pkg}-linux-x64": "${version}",
    "${pkg}-linux-arm64": "${version}"
  },
  "publishConfig": { "access": "public", "provenance": true }
}
JSON

cat > "${dir}/bin/${name}.js" <<JS
#!/usr/bin/env node
// Thin launcher: resolves the platform package that carries the real static
// binary (fs engine compiled in) and exec-replaces into it. The npm layer
// adds no runtime; node exits as soon as the binary takes over.
"use strict";
const { spawnSync } = require("node:child_process");

const NAME = "${name}";
const PLATFORMS = {
  "darwin arm64": "${pkg}-darwin-arm64",
  "darwin x64": "${pkg}-darwin-x64",
  "linux x64": "${pkg}-linux-x64",
  "linux arm64": "${pkg}-linux-arm64",
};

const key = \`\${process.platform} \${process.arch}\`;
const pkg = PLATFORMS[key];
if (!pkg) {
  console.error(\`\${NAME}: unsupported platform \${key}\`);
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(\`\${pkg}/bin/\${NAME}\`);
} catch {
  console.error(
    \`\${NAME}: platform package \${pkg} is not installed.\\n\` +
      "Your package manager likely skipped optional dependencies " +
      "(--no-optional / --omit=optional). Reinstall without that flag.",
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(\`\${NAME}: \${result.error.message}\`);
  process.exit(1);
}
process.exit(result.status ?? 1);
JS
chmod 0755 "${dir}/bin/${name}.js"
echo "$dir"
