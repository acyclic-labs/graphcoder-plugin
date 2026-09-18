# Contributing

## Build and test

```sh
cargo build
cargo test
```

Acceptance suites (end-to-end, run against a real repo) live in `tests/acceptance/`:

```sh
tests/acceptance/run-all.sh
```

Individual suites (`journey.sh`, `timeline.sh`, `forks.sh`, `merge.sh`, `safe-mode.sh`, etc.) can
be run with `bash tests/acceptance/<suite>.sh` if you're iterating on one feature.

The `*-e2e.sh` suites drive the real host CLIs (Claude Code, Codex, cursor-agent, OpenCode) and
cost a model session each; they run only with `ACYCLIC_E2E=1`. Run them, or the manual
checklist in `docs/manual-testing.md`, whenever you touch an adapter in `install.rs` or a host
ships a new release — CI cannot see a host silently ignoring our config.

## Commit messages

- Summary line: imperative mood ("Add", "Fix", "Rename", not "Added"/"Fixes"), no trailing
  period, aim for 72 characters or fewer.
- Blank line, then a body explaining *why* the change is needed and what it does — not a
  restatement of the diff. Bullet points are fine for a commit that bundles several related
  pieces; a one-line summary needs no body at all.
- Reference an issue or PR number when the commit closes or relates to one (`Fixes #12`,
  `See #7`).
- No AI attribution trailers or "Generated with ..." lines — see "AI-assisted contributions"
  below.

## Before opening a PR

Squash your branch down to one commit (or a small number of logically distinct commits) before
requesting review — `git rebase -i` or `git reset --soft <base> && git commit`. Reviewers should
see the change you're proposing, not the history of how you got there; merges into `main` are
squash-only regardless (see repo settings), but a clean branch makes review itself easier.

Run these locally — CI enforces all of them:

```sh
scripts/check-product-name.sh   # the public name only comes from product.toml
scripts/check-no-secrets.sh     # no forbidden files or credential patterns
scripts/check-code-quality.sh   # line width, TODO(topic) format, comment-block length, duplication
cargo deny check                # dependency licenses, advisories, bans
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo llvm-cov --workspace --all-features --fail-under-lines 48   # needs cargo-llvm-cov
```

Or all of it, in CI's order, with one summary at the end: `scripts/ci-local.sh` (add
`--no-acceptance` to skip the slow end-to-end suite while iterating).

## Code quality rules

Beyond rustfmt and clippy's defaults, the workspace enables a lint set in `Cargo.toml`
(`[workspace.lints.clippy]`, thresholds in `clippy.toml`) and `scripts/check-code-quality.sh`
guards what those can't express. The rules, and why each exists:

- **Functions under 100 lines, cognitive complexity under 30.** A dispatcher that is one arm per
  protocol op may carry `#[allow(clippy::too_many_lines, reason = "...")]`; anything else that
  trips it should be split.
- **No identical match arms, no `match` for a single pattern, `let ... else` over manual
  matches.** Copy-pasted arms were the recurring review finding — the lints catch the next one.
- **No `todo!`, `unimplemented!`, or `dbg!` in committed code.** Open work is a `TODO(topic):`
  comment (the script rejects a bare `TODO`), so it names who or what it is waiting on.
- **No hidden panics in non-test code.** `unwrap`, `expect`, `panic!`, `slice[i]`, and
  `&text[a..b]` are lint errors outside tests (`clippy::unwrap_used`, `expect_used`, `panic`,
  `indexing_slicing`, `string_slice`). Reach for `?`, `get`, `let ... else`, `starts_with`,
  `saturating_sub`. The few documented exceptions (a build script failing the build, the
  pipeline thread that nothing can run without) carry `#[allow(..., reason = "...")]`.
- **No lossy `as` casts** between integer widths or signs (`cast_possible_truncation`,
  `cast_sign_loss`, `cast_possible_wrap`, `cast_precision_loss`, `cast_lossless`). Use
  `u64::from`, `i64::try_from(x).unwrap_or(i64::MAX)`, or an `allow` that says why the value
  is in range. `acyclic::unix_now()` and `short_hex()` exist so the two most common
  cases are written once.
- **Closed sets are enums, not strings.** Anything the CLI parses or the wire carries with a
  fixed vocabulary — checkpoint kinds, hook events, host names, restore actions, diff change
  kinds — is an enum (`clap::ValueEnum` on the CLI side, serde `rename_all = "snake_case"` on
  the wire), so an unknown value is rejected at the boundary and a `match` on it is exhaustive.
- **`unsafe` is opt-in per function.** `unsafe_code` is a warning; each block sits in a function
  carrying `#[allow(unsafe_code, reason = "...")]` that names the invariant, so `grep allow(unsafe`
  lists every one.
- **Lines: 120 columns in Rust, 200 in shell.** rustfmt wraps code at 100 but leaves strings
  and comments alone; split long format strings with `\` continuations.
- **Comment blocks under 30 lines.** A longer one is a design note (`docs/design/`) or a sign
  the code needs to be simpler, not explained harder. Comments say *why*; the code says what.
- **Duplication under 3% of tokens** (`jscpd`, config in `.jscpd.json`). Extract a helper
  before the third copy.
- **Line coverage floor of 48%** from `cargo test` alone. The daemon, client, and MCP server
  are exercised by the acceptance scripts, not unit tests, so they read as 0% — the floor moves
  up as unit coverage of those grows.

## Product naming

The public product name is defined once, in `product.toml`, and threaded through everywhere else:
`product::NAME` in Rust, `scripts/product.sh` in shell. Never hardcode the name as a literal
string or path in source — `scripts/check-product-name.sh` fails CI if it drifts. Crate names
(`acyclic`, `acyclic-qual`) are internal identifiers and are
exempt from this check.

## Design context

`docs/design/` has the design docs and implementation notes behind the bigger features (Rewind,
Timeline, Forks, Safe Mode). Worth a skim before working on any of them — they capture the
tradeoffs and constraints that shaped the current architecture, including a few (like snapshot
GC) that are intentionally deferred.

## Reporting bugs and security issues

Open a GitHub issue for regular bugs. For security vulnerabilities, follow `SECURITY.md` instead
of filing a public issue.

## AI-assisted contributions

Using AI tools to help write a PR is fine, but the commit and PR itself should read as yours: no
AI attribution trailers (`Co-Authored-By` naming an agent), "Generated with ..." lines, or agent
session links in commit messages or PR descriptions. See `CLAUDE.md` for the equivalent
instruction to agents themselves. Every PR needs review and approval from a code owner
(`.github/CODEOWNERS`) before it can merge.
