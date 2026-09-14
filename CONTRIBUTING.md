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
cargo deny check                # dependency licenses, advisories, bans
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Product naming

The public product name is defined once, in `product.toml`, and threaded through everywhere else:
`product::NAME` in Rust, `scripts/product.sh` in shell. Never hardcode the name as a literal
string or path in source — `scripts/check-product-name.sh` fails CI if it drifts. Crate names
(`acyclic`, `acyclic-engine`, `acyclic-proto`, `acyclic-qual`) are internal identifiers and are
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
