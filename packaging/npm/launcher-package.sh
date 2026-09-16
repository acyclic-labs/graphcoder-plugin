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
    "${pkg}-linux-arm64": "${version}",
    "${pkg}-win32-x64": "${version}"
  },
  "publishConfig": { "access": "public", "provenance": true }
}
JSON

cat > "${dir}/bin/${name}.js" <<JS
#!/usr/bin/env node
// Thin launcher: resolves the platform package that carries the real static
// binary (fs engine compiled in) and runs it with this process's stdio, then
// exits with whatever it exited with. Node has no execve, so it stays as an
// idle parent for the child's lifetime; it adds no runtime of its own.
"use strict";
const { spawnSync } = require("node:child_process");

const NAME = "${name}";
const PLATFORMS = {
  "darwin arm64": "${pkg}-darwin-arm64",
  "darwin x64": "${pkg}-darwin-x64",
  "linux x64": "${pkg}-linux-x64",
  "linux arm64": "${pkg}-linux-arm64",
  "win32 x64": "${pkg}-win32-x64",
};

// The Windows platform package ships acyclic.exe; every other target ships a
// suffixless binary.
const EXE = process.platform === "win32" ? ".exe" : "";

const key = \`\${process.platform} \${process.arch}\`;
const pkg = PLATFORMS[key];
if (!pkg) {
  console.error(\`\${NAME}: unsupported platform \${key}\`);
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(\`\${pkg}/bin/\${NAME}\${EXE}\`);
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
// A signalled child reports status null; shells encode that as 128+signum, so
// reporting it the same way makes a Ctrl-C through the launcher look to the
// calling shell exactly like a Ctrl-C straight into the binary.
if (result.signal) {
  process.exit(128 + (require("node:os").constants.signals[result.signal] ?? 0));
}
process.exit(result.status ?? 1);
JS
chmod 0755 "${dir}/bin/${name}.js"
echo "$dir"
