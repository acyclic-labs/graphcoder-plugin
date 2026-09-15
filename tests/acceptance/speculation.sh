#!/usr/bin/env bash
# Speculation (docs/design/08-speculation.md): the daemon computes the
# previous-session brief when a session ENDS, so the next session start
# serves it from cache instead of paying N pipeline diffs for it.
#
# The two claims that matter are opposites, and both are here:
#   - a matching request is served from the cache, and
#   - a request against a tree that has MOVED is a miss, not a stale answer.
# The second is the whole correctness argument: a result is keyed by the
# generation it describes, so staleness is unrepresentable rather than
# checked for.
#
# Costs nothing and needs no credentials: this is the free half (precompute
# and claim). The model-run half has its own gate.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

spec_db() { echo "$STORES"/*/spec.db; }

# The rollup `status` prints when speculation is on. Asserting against the
# product's own reporting rather than the daemon log keeps this honest: if
# the number a user reads is wrong, the test fails.
rollup() { acy status | grep "24h:"; }

# ---------------------------------------------------------------- S1: off
# The default must be what it was before the feature existed: several
# acceptance scripts read `status` output.
setup_repo
acy init >/dev/null || fail "init"
acy status | grep -q "speculation" && fail "S1: status mentions speculation when it is off"
[ -f "$(spec_db)" ] && fail "S1: spec.db exists with speculation disabled"
acy stop >/dev/null || fail "S1: stop"
sleep 1
pass "S1: off by default — no cache, no status line"

# ------------------------------------------------------- S2: precompute
# A config file of its own, never the repo's checked-in one: this setting
# can spend a developer's money, so it is not team policy to inherit.
SPEC_CONFIG="$WORK/speculate.toml"
cat > "$SPEC_CONFIG" <<'TOML'
enabled = true
kinds = ["brief"]
TOML
export ACYCLIC_SPECULATE_CONFIG="$SPEC_CONFIG"

acy status >/dev/null || fail "S2: restart daemon with speculation on"
acy status | grep -q "^speculation:" || fail "S2: status does not report speculation"
acy status | grep -q "no model runs" \
  || fail "S2: status must say no model runs when no command is configured"

# A session with real work in it, then a rewind, so the brief has an
# abandoned branch to cost — that is the expensive part being precomputed.
acy session-start s1 --host claude-code >/dev/null || fail "S2: session-start"
printf 'one\n' > "$R/src/main.rs"
acy checkpoint --wait --session-id s1 -m "first" >/dev/null || fail "S2: checkpoint"
BASE="$(acy timeline --limit 1 | awk 'NR==1 {print $1}' | tr -d '#')"
printf 'two\n' > "$R/src/main.rs"
acy checkpoint --wait --session-id s1 -m "second" >/dev/null || fail "S2: checkpoint"
acy rewind "$BASE" --yes >/dev/null || fail "S2: rewind"
acy session-end s1 >/dev/null || fail "S2: session-end"
settle 3

rollup | grep -q "1 run(s)" \
  || fail "S2: session end did not precompute the brief; rollup: $(rollup)"
pass "S2: a session ending precomputes the next session's brief"

# ------------------------------------------------------------ S3: claim
BRIEF="$(acy brief --current s2)" || fail "S3: brief"
rollup | grep -q "1 of 1 claimed" \
  || fail "S3: brief was recomputed rather than claimed; rollup: $(rollup)"
# The claimed answer must be the real one, not an empty shell.
grep -q "abandoned branch" <<<"$BRIEF" \
  || fail "S3: claimed brief lost its content: $BRIEF"
pass "S3: the next session start serves the precomputed brief"

# ------------------------------------------------- S4: a moved tree misses
# The correctness proof. Move the tree and ask again: the key no longer
# matches, so the cached brief is unreachable and the answer is computed
# fresh. Serving the old prose against the new tree is the bug this design
# makes unrepresentable.
printf 'three\n' > "$R/src/main.rs"
acy checkpoint --wait -m "moves the tree" >/dev/null || fail "S4: checkpoint"
MOVED="$(acy brief --current s2)" || fail "S4: brief after the tree moved"
rollup | grep -q "1 of 2 claimed" \
  || fail "S4: a moved tree must miss; rollup: $(rollup)"
# And the recomputed answer describes the new tree rather than the cached one.
grep -q "tree has moved since" <<<"$MOVED" \
  || fail "S4: recomputed brief should report drift: $MOVED"
pass "S4: a tree that moved is a miss, not a stale answer"

# --------------------------------------------- S5: the miss is memoized
# Asking again over the same tree hits, though nothing scheduled it.
acy brief --current s2 >/dev/null || fail "S5: brief"
rollup | grep -q "2 of 3 claimed" \
  || fail "S5: a recomputed brief should be cached; rollup: $(rollup)"
pass "S5: a miss caches its own result for the next asker"

# ------------------------------------------------------ S6: clean teardown
acy stop >/dev/null || fail "S6: stop"
sleep 1
pgrep -f "acyclic.*__daemon.*$R" >/dev/null && fail "S6: daemon still running after stop"
pass "S6: the scheduler stops with the daemon"
