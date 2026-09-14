#!/usr/bin/env bash
# Local release driver: the same steps as .github/workflows/release.yml, run
# from a developer machine. Intended for first deploys and for rehearsing
# against a local registry before anything reaches npmjs.com.
#
#   packaging/npm/release-local.sh build   [--all]           build binaries (host arch; --all adds the other darwin arch and linux via docker)
#   packaging/npm/release-local.sh pack                      assemble platform packages + launcher, npm pack each into dist/npm/
#   packaging/npm/release-local.sh publish [--registry URL] [--dry-run]
#   packaging/npm/release-local.sh verify  [--registry URL]  clean install of the launcher from the registry, run --version
#   packaging/npm/release-local.sh all     [--registry URL] [--dry-run]   build (host) + pack + publish + verify
#
# Rehearsal against a throwaway registry:
#   npx verdaccio &                                   # http://localhost:4873
#   npm adduser --registry http://localhost:4873      # any user/password
#   packaging/npm/release-local.sh all --registry http://localhost:4873
#
# Real publish (host arch only unless you ran `build --all` first):
#   npm login                                         # once; needs publish rights on @acyclic-labs
#   packaging/npm/release-local.sh publish --dry-run  # look at what would go out
#   packaging/npm/release-local.sh publish
#
# Auth, two ways, never written to the repo:
#   1. `npm login` once; npm keeps the session in ~/.npmrc.
#   2. An NPM_TOKEN environment variable, e.g. injected by a secrets manager:
#        infisical run --env=prod -- packaging/npm/release-local.sh publish
#      The script points npm at a temporary npmrc that references ${NPM_TOKEN}
#      by name only; the token value itself is never written anywhere.
# Provenance is CI-only and is switched off for local publishes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NPM_DIR="$ROOT/packaging/npm"
DIST="$ROOT/dist"
BIN_DIR="$DIST/bin"
PKG_DIR="$DIST/npm-packages"
TGZ_DIR="$DIST/npm"
LAUNCHER="$NPM_DIR/acyclic"

export CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_PROFILE_RELEASE_STRIP=symbols

# If NPM_TOKEN is set, hand it to npm through a throwaway user config that
# contains only an env-var reference. npm expands ${NPM_TOKEN} at read time.
if [ -n "${NPM_TOKEN:-}" ]; then
  _npmrc="$(mktemp)"
  trap 'rm -f "$_npmrc"' EXIT
  _host="${NPM_REGISTRY_HOST:-registry.npmjs.org}"
  printf '//%s/:_authToken=${NPM_TOKEN}\n' "$_host" > "$_npmrc"
  export NPM_CONFIG_USERCONFIG="$_npmrc"
fi

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

version() {
  awk '/^\[workspace.package\]/{f=1;next} /^\[/{f=0} f && /^version/ {gsub(/[" ]/,"",$3); print $3}' "$ROOT/Cargo.toml"
}

check_versions() {
  local v="$1"
  local npm_v
  npm_v="$(node -p "require('$LAUNCHER/package.json').version")"
  [ "$v" = "$npm_v" ] || die "Cargo.toml is $v but packaging/npm/acyclic/package.json is $npm_v"
  local p dep
  for p in $(node -p "Object.keys(require('$LAUNCHER/package.json').optionalDependencies).join(' ')"); do
    dep="$(node -p "require('$LAUNCHER/package.json').optionalDependencies['$p']")"
    [ "$dep" = "$v" ] || die "optionalDependency $p is pinned to $dep, expected $v"
  done
  log "version $v is consistent across Cargo.toml and the launcher"
}

host_target() {
  case "$(uname -s) $(uname -m)" in
    "Darwin arm64")  echo "darwin arm64 aarch64-apple-darwin" ;;
    "Darwin x86_64") echo "darwin x64 x86_64-apple-darwin" ;;
    "Linux x86_64")  echo "linux x64 x86_64-unknown-linux-gnu" ;;
    "Linux aarch64") echo "linux arm64 aarch64-unknown-linux-gnu" ;;
    *) die "unsupported host $(uname -s) $(uname -m)" ;;
  esac
}

build_one() {
  # build_one <os> <cpu> <rust-target>
  local os="$1" cpu="$2" target="$3" v
  v="$(version)"
  log "building $os $cpu ($target)"
  rustup target add "$target" >/dev/null
  (cd "$ROOT" && cargo build --release --locked -p acyclic --target "$target")
  local bin="$ROOT/target/$target/release/acyclic"
  mkdir -p "$BIN_DIR"
  cp "$bin" "$BIN_DIR/acyclic-$os-$cpu"
  # Only smoke-test what this machine can execute.
  if [ "$os" = "$(uname -s | tr '[:upper:]' '[:lower:]')" ]; then
    local out
    out="$("$BIN_DIR/acyclic-$os-$cpu" --version 2>/dev/null || true)"
    if [ -n "$out" ]; then
      [[ "$out" == *"$v"* ]] || die "binary reports '$out', expected $v"
      log "smoke test: $out"
    fi
  fi
  (cd "$BIN_DIR" && shasum -a 256 "acyclic-$os-$cpu" > "acyclic-$os-$cpu.sha256")
}

build_linux_docker() {
  # build_linux_docker <cpu> <docker-platform> <rust-target>
  local cpu="$1" platform="$2" target="$3"
  command -v docker >/dev/null || { log "docker not found, skipping linux $cpu"; return 0; }
  log "building linux $cpu in docker ($platform)"
  mkdir -p "$BIN_DIR"
  docker run --rm --platform "$platform" \
    -v "$ROOT:/src:ro" \
    -v "acyclic-release-cargo-$cpu:/cargo" \
    -v "acyclic-release-target-$cpu:/build" \
    -v "$BIN_DIR:/out" \
    -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/build \
    -e CARGO_NET_GIT_FETCH_WITH_CLI=true -e CARGO_PROFILE_RELEASE_STRIP=symbols \
    rust:1-bookworm bash -eu -o pipefail -c "
      cd /src
      cargo build --release --locked -p acyclic --target $target
      cp /build/$target/release/acyclic /out/acyclic-linux-$cpu
      /out/acyclic-linux-$cpu --version
    "
  (cd "$BIN_DIR" && shasum -a 256 "acyclic-linux-$cpu" > "acyclic-linux-$cpu.sha256")
}

cmd_build() {
  local all=false
  [ "${1:-}" = "--all" ] && all=true
  check_versions "$(version)"
  read -r os cpu target <<<"$(host_target)"
  build_one "$os" "$cpu" "$target"
  if $all; then
    if [ "$os" = darwin ]; then
      if [ "$cpu" = arm64 ]; then build_one darwin x64 x86_64-apple-darwin; else build_one darwin arm64 aarch64-apple-darwin; fi
      build_linux_docker x64   linux/amd64 x86_64-unknown-linux-gnu
      build_linux_docker arm64 linux/arm64 aarch64-unknown-linux-gnu
    else
      [ "$cpu" = x64 ] && build_linux_docker arm64 linux/arm64 aarch64-unknown-linux-gnu
      [ "$cpu" = arm64 ] && build_linux_docker x64 linux/amd64 x86_64-unknown-linux-gnu
      log "darwin binaries can only be built on macOS; skipping"
    fi
  fi
  log "binaries in $BIN_DIR:"
  ls -l "$BIN_DIR"
}

cmd_pack() {
  local v; v="$(version)"
  check_versions "$v"
  [ -d "$BIN_DIR" ] || die "no binaries; run build first"
  rm -rf "$PKG_DIR" "$TGZ_DIR"
  mkdir -p "$PKG_DIR" "$TGZ_DIR"
  (cd "$BIN_DIR" && shasum -a 256 -c ./*.sha256)
  local bin os cpu
  for bin in "$BIN_DIR"/acyclic-*-*; do
    [[ "$bin" == *.sha256 ]] && continue
    os="$(basename "$bin" | cut -d- -f2)"; cpu="$(basename "$bin" | cut -d- -f3)"
    "$NPM_DIR/platform-package.sh" "$os" "$cpu" "$v" "$bin" "$PKG_DIR" >/dev/null
    (cd "$PKG_DIR/@acyclic-labs/plugin-$os-$cpu" && npm pack --ignore-scripts --pack-destination "$TGZ_DIR" >/dev/null)
    log "packed @acyclic-labs/plugin-$os-$cpu@$v"
  done
  (cd "$LAUNCHER" && npm pack --ignore-scripts --pack-destination "$TGZ_DIR" >/dev/null)
  log "packed @acyclic-labs/plugin@$v"
  ls -l "$TGZ_DIR"
}

cmd_publish() {
  local registry="" dry=false
  while [ $# -gt 0 ]; do
    case "$1" in
      --registry) registry="$2"; shift 2 ;;
      --dry-run)  dry=true; shift ;;
      *) die "unknown flag $1" ;;
    esac
  done
  local v; v="$(version)"
  [ -d "$PKG_DIR" ] || die "no packages; run pack first"
  if [ -n "${NPM_TOKEN:-}" ] && [ -n "$registry" ]; then
    # Token was written for registry.npmjs.org; rewrite for the chosen host.
    printf '//%s/:_authToken=${NPM_TOKEN}\n' "$(sed -E 's#^[a-z]+://##; s#/$##' <<<"$registry")" > "$NPM_CONFIG_USERCONFIG"
  fi
  local flags=(--access public --provenance=false --ignore-scripts)
  [ -n "$registry" ] && flags+=(--registry "$registry")
  $dry && flags+=(--dry-run)

  local who
  who="$(npm whoami ${registry:+--registry "$registry"} 2>/dev/null || true)"
  [ -n "$who" ] || die "not logged in to ${registry:-npmjs.com}; run: npm login${registry:+ --registry $registry}"
  log "publishing as '$who' to ${registry:-https://registry.npmjs.org}$($dry && echo " (dry run)")"

  local pkg name
  for pkg in "$PKG_DIR"/@acyclic-labs/plugin-*; do
    name="$(node -p "require('$pkg/package.json').name")"
    if ! $dry && npm view "$name@$v" version ${registry:+--registry "$registry"} >/dev/null 2>&1; then
      log "$name@$v already published, skipping"; continue
    fi
    log "publish $name@$v"
    (cd "$pkg" && npm publish "${flags[@]}")
  done

  if ! $dry && npm view "@acyclic-labs/plugin@$v" version ${registry:+--registry "$registry"} >/dev/null 2>&1; then
    log "launcher $v already published, skipping"
  else
    log "publish @acyclic-labs/plugin@$v"
    (cd "$LAUNCHER" && npm publish "${flags[@]}")
  fi
}

cmd_verify() {
  local registry=""
  [ "${1:-}" = "--registry" ] && registry="$2"
  local v; v="$(version)"
  local tmp; tmp="$(mktemp -d)"
  log "clean install of @acyclic-labs/plugin@$v into $tmp"
  (cd "$tmp" && npm init -y >/dev/null && npm install --ignore-scripts ${registry:+--registry "$registry"} "@acyclic-labs/plugin@$v")
  local out
  out="$("$tmp/node_modules/.bin/acyclic" --version)"
  [[ "$out" == *"$v"* ]] || die "installed binary reports '$out', expected $v"
  log "installed launcher resolved to: $(ls "$tmp/node_modules/@acyclic-labs/")"
  log "verify ok: $out"
  rm -rf "$tmp"
}

case "${1:-}" in
  build)   shift; cmd_build "$@" ;;
  pack)    shift; cmd_pack ;;
  publish) shift; cmd_publish "$@" ;;
  verify)  shift; cmd_verify "$@" ;;
  all)
    shift; cmd_build; cmd_pack; cmd_publish "$@"
    case " $* " in *" --dry-run "*) log "dry run: skipping verify" ;; *) cmd_verify "$@" ;; esac ;;
  *) sed -n '2,25p' "$0"; exit 1 ;;
esac
