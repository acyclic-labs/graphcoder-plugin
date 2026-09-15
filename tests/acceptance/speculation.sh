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

# ================================================================= model runs
# The half that spends money, driven by a stub so it costs nothing. What is
# under test is the runner's contract, not any model's output: it is
# pre-fired at a turn boundary, it is bounded, it is killable, and it never
# produces a summary on demand.
# Absolute: the child runs in an empty scratch dir, so a relative command
# would be resolved against that and not found.
STUB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/stub-model.sh"

# A conversation turn, as the host's UserPromptSubmit hook reports one.
# There is no CLI verb for this on purpose: turns come from the host.
turn() {
  printf '{"session_id":"%s","prompt":"%s"}' "$1" "$2" \
    | "$BIN" --repo "$R" hook user-prompt >/dev/null
}

# restart_with <toml-body>: a fresh daemon reading a new speculation config.
restart_with() {
  acy stop >/dev/null 2>&1 || true
  sleep 1
  printf '%s\n' "$1" > "$SPEC_CONFIG"
  acy status >/dev/null || fail "restart with: $1"
}

# ------------------------------------------------ S7: two gates, not one
# Naming summaries without a command must not start spending: it takes two
# deliberate settings, so no single boolean can put anyone on the meter.
restart_with 'enabled = true
kinds = ["brief", "summary"]'
acy status | grep -q "no model runs" \
  || fail "S7: summaries without a command must not count as spending"
pass "S7: enabling summaries without a command does not spend"

restart_with "enabled = true
kinds = [\"brief\", \"summary\"]
command = [\"$STUB\"]
min_run_interval_ms = 0"
acy status | grep -q "precompute + model runs" \
  || fail "S8: status should report that model runs are on"
pass "S8: a configured command turns model runs on, and status says so"

# ----------------------------------------- S9: pre-fired at a turn boundary
# Turn 2 starting is what says turn 1 finished, and that is when the
# summarizer runs — with nobody waiting on it.
acy session-start s9 --host claude-code >/dev/null || fail "S9: session-start"
turn s9 "add the retry helper"
printf 'retry\n' > "$R/src/retry.rs"
acy checkpoint --wait --session-id s9 -m "turn 1 work" >/dev/null || fail "S9: checkpoint"
turn s9 "now the tests"
settle 3

SUMMARY="$(acy summary --session s9 --turn 1)" || fail "S9: summary"
grep -q "stub summary of" <<<"$SUMMARY" \
  || fail "S9: turn 1 was not summarised ahead of the request: $SUMMARY"
# It must say where the words came from: this is generated prose.
grep -q "speculated summary" <<<"$SUMMARY" \
  || fail "S9: a served summary must name its source: $SUMMARY"
pass "S9: a finished turn is summarised before anything asks"

# ------------------------------------------- S10: never produced on demand
# An unsummarised turn reports that plainly. Producing one here would bill
# whoever asked, and asking is not consent to spend.
MISSING="$(acy summary --session s9 --turn 99)" || fail "S10: summary"
grep -q "no summary" <<<"$MISSING" \
  || fail "S10: an unknown turn must not be produced on demand: $MISSING"
pass "S10: a missing summary is reported, never produced on demand"

# ---------------------------------------------------- S11: a run is bounded
# A model command that never returns must not leave a process behind. The
# (That the kill reaches a run's own children — the case that matters for a
# real agent CLI, which is a runtime spawning subprocesses — is proven by
# `spec_runner::tests::a_timeout_kills_the_children_too`, where the process
# tree can be observed precisely.)
#
# The pattern is anchored to the child's exact argv, for two reasons: an
# unanchored `pgrep -f` also matches any shell whose command line happens to
# mention it (including the one running this suite), and `sh -c` execs a
# simple command in place, so what is actually running is `sleep 97`.
running() { pgrep -f '^sleep 97$' >/dev/null; }

restart_with "enabled = true
kinds = [\"summary\"]
command = [\"/bin/sh\", \"-c\", \"sleep 97\"]
timeout_ms = 2500
kill_grace_ms = 200
min_run_interval_ms = 0"
acy session-start s11 --host claude-code >/dev/null || fail "S11: session-start"
turn s11 "slow one"
printf 'slow\n' > "$R/src/slow.rs"
acy checkpoint --wait --session-id s11 -m "turn 1" >/dev/null || fail "S11: checkpoint"
turn s11 "next"
# Poll rather than sleep a fixed time: when the run starts depends on the
# pipeline, and when it dies depends on the timeout.
wait_for() { for _ in $(seq 1 60); do "$@" && return 0; sleep 0.2; done; return 1; }
not_running() { ! running; }
wait_for running || fail "S11: the run never started; rollup: $(rollup)"
wait_for not_running || fail "S11: the run's child survived the timeout"
# Poll for the record too: the child dies on SIGTERM, but the outcome is
# written after the kill grace, so a single check here races the daemon.
recorded_timeout() { rollup | grep -q "timeout"; }
wait_for recorded_timeout \
  || fail "S11: the timeout was not recorded; rollup: $(rollup)"
pass "S11: a run that overruns is killed and recorded"

acy stop >/dev/null || fail "S12: stop"
sleep 1
running && fail "S12: a model run outlived the daemon"
pass "S12: no model run outlives the daemon"
