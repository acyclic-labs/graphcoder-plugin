# Releasing to npm

`.github/workflows/release.yml` builds the `acyclic` binary for four targets,
attaches them to a GitHub release, and publishes five npm packages: the
launcher `@acyclic-labs/plugin` and one `@acyclic-labs/plugin-<os>-<cpu>`
platform package per target. Users run `npm i -g @acyclic-labs/plugin`; npm
installs only the platform package that matches their machine.

## One-time setup

### 1. npm organisation and token

1. Create the `acyclic-labs` organisation on npmjs.com and enable
   **two-factor authentication required** for the org.
2. Create a **granular access token**, not a classic one:
   - Packages and scopes: read and write, restricted to the `@acyclic-labs`
     scope only.
   - Organisations: no access.
   - Expiry: 90 days or less. Rotate it; do not create a token that never expires.
   - Bypass 2FA: **on**, otherwise CI cannot publish. This is why the scope
     restriction and short expiry matter.
3. Never commit the token, paste it in a `.npmrc`, or pass it on a command
   line. It goes in GitHub only, as described next.

Preferred alternative: npm **trusted publishing**. On each package's settings
page, add a trusted publisher for this repository and the workflow file
`release.yml`. npm then accepts the workflow's OIDC identity and no token is
needed at all. The workflow already requests `id-token: write`, so once
trusted publishing is configured you can delete the `NPM_TOKEN` secret.
Note that trusted publishing can only be added to a package that already
exists, so the first publish of each package needs the token.

### 2. GitHub environment

1. Repository settings, Environments, create one named `npm`.
2. Add **required reviewers** (at least one maintainer). The publish job
   pauses until a reviewer approves, and the token is never exposed before
   that.
3. Restrict deployment branches and tags to tags matching `v*`.
4. Add the environment secret `NPM_TOKEN` with the granular token. Do not add
   it as a repository-level secret, or every workflow could read it.

### 3. Branch and tag protection

- Protect `main`: require pull requests and passing `ci`.
- Add a tag protection rule (or ruleset) for `v*` so only maintainers can
  push release tags. Release tags are the only thing that can trigger a
  real publish.

## Local release (current practice)

Until the GitHub workflow is exercised, releases are cut from a maintainer's
Mac with `packaging/npm/release-local.sh`. The npm token lives in Infisical
(Development environment, folder `/auth`, secret `NPM_TOKEN`) and is injected
into the script's environment only for the publish step. The script hands it
to npm through a temporary npmrc that references `${NPM_TOKEN}` by name, so
the value is never written to disk. Do not run `infisical secrets` in a shared
terminal: it prints values.

```sh
infisical login                                  # once per machine
packaging/npm/release-local.sh build             # add --all for darwin x64 + linux via docker
packaging/npm/release-local.sh pack
infisical run --env=dev --path=/auth -- packaging/npm/release-local.sh publish --dry-run
infisical run --env=dev --path=/auth -- packaging/npm/release-local.sh publish
packaging/npm/release-local.sh verify            # clean install from npmjs, runs --version
```

To rehearse against a throwaway registry first, start `npx verdaccio` and pass
`--registry http://localhost:4873` to `publish` and `verify`.

New packages can take several minutes to become readable on the registry
after a successful publish, so a 404 from `verify` immediately afterwards is
expected. Retry before assuming the publish failed.

## Cutting a release (GitHub workflow)

1. Bump the version in `Cargo.toml` under `[workspace.package]`. That is the
   only version source: the npm launcher and platform packages are generated
   from it at publish time (`packaging/npm/launcher-package.sh`,
   `platform-package.sh`). The public name and npm package name come from
   `product.toml`; the `verify` job runs `scripts/check-product-name.sh`.

   The bump is not optional. `Cargo.toml` still reads `0.0.1` and
   `@acyclic-labs/plugin@0.0.1` is already on npm, so tagging `v0.0.1` would
   cut a GitHub release whose npm half is silently skipped as
   already-published. Go to `0.0.2` or later.
2. Merge that change to `main` and wait for `ci` to pass.
3. Dry run first. Actions, release, Run workflow, leave `dry_run` checked.
   This builds all four binaries, attests them, verifies the attestations,
   and runs `npm publish --dry-run`. It creates no release and publishes
   nothing.
4. Tag and push:

   ```sh
   git tag -a v0.0.2 -m "v0.0.2"   # must match Cargo.toml exactly
   git push origin v0.0.2
   ```

5. Approve the `npm` environment deployment when the workflow pauses.
6. The workflow ends with a fresh `npm install` of the launcher from the
   public registry and runs `acyclic --version`. If that step is green, the
   release is live.
7. `scripts/install.sh` resolves `releases/latest/download/` by default, so
   the curl installer picks the new release up with no further step. Until a
   release exists it has nothing to resolve and exits telling the user to use
   npm — the first tag is what turns that path on. Rehearse it offline first:
   `scripts/install-smoke.sh dist/bin` runs the installer on a bare Debian
   container against a local copy of the release assets and drives init →
   checkpoint → rewind. Afterwards, confirm the live path end to end:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/acyclic-labs/graphcoder-plugin/main/scripts/install.sh | sh
   ```

## What the workflow enforces

| Control | Where |
|---|---|
| Runs only on `v*` tags or manual dispatch; never on PRs | `on:` block |
| Every action pinned to a commit SHA | each `uses:` line |
| Default `GITHUB_TOKEN` is read-only; jobs request extra scopes individually | `permissions:` blocks |
| Checkout does not persist credentials | `persist-credentials: false` |
| Build uses `--locked`, so Cargo.lock is authoritative | build step |
| Binary must report the release version before it is kept | smoke test step |
| SLSA provenance attestation for every binary | `attest-build-provenance` |
| SPDX SBOM from `Cargo.lock` per target, attested against its binary; one copy attached to the release and listed in `SHA256SUMS` | `sbom-action` + `attest-sbom` |
| Licenses, advisories, and sources checked against `deny.toml` on every push | `ci.yml` `deny` job |
| Publish job re-verifies attestations and checksums before packaging | verify step |
| npm token visible only to the `npm` environment job | `environment: npm` |
| npm lifecycle scripts disabled everywhere | `npm config set ignore-scripts true` and `--ignore-scripts` |
| Packages published with npm provenance | `--provenance` |
| Platform packages before launcher; launcher never points at missing deps | job step order |
| Re-runs are safe: versions already on the registry are skipped | `npm view` check |
| Post-publish install from the public registry with no auth | last step |

## Recovering from a bad release

npm allows unpublishing a version within 72 hours if nothing depends on it,
but only from an interactive `npm login` session with 2FA. An automation
token gets a 403, so the release script cannot unpublish. Do it from the
package's settings page on npmjs.com, or `npm login` locally first. After 72
hours, publish a fixed patch version and `npm deprecate` the bad one, which
the token can do.

The original packages `@acyclic-labs/acyclic` and
`@acyclic-labs/acyclic-darwin-arm64` (0.0.1) were published before the rename
to `@acyclic-labs/plugin`. They are deprecated with a pointer to the new name
and can be deleted from the npmjs.com UI.
Never reuse a version number. If the token may have leaked, revoke it on
npmjs.com first, then rotate the GitHub environment secret.
