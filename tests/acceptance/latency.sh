#!/usr/bin/env bash
# Hook-path latency gate: the enqueue-ack checkpoint round trip (what a
# PostToolUse hook pays) must stay under budget with a warm daemon on a
# populated tree. Budget and corpus size are tunable; CI uses the defaults.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

FILES="${ACYCLIC_LAT_FILES:-20000}"
CORPUS_MB="${ACYCLIC_LAT_MB:-256}"
SAMPLES="${ACYCLIC_LAT_SAMPLES:-30}"
BUDGET_MS="${ACYCLIC_LAT_BUDGET_MS:-100}"

setup_repo
"$QUAL" corpus "$R" "$FILES" "$CORPUS_MB" >/dev/null || fail "corpus generation"
acy init >/dev/null || fail "init (includes baseline of the corpus)"

# Warm-up round, then timed samples. Each round touches a file so the
# checkpoint has real work queued behind the ack.
acy checkpoint --kind post >/dev/null

P95="$(python3 - "$BIN" "$R" "$SAMPLES" <<'PY'
import subprocess, sys, time
bin_path, repo, samples = sys.argv[1], sys.argv[2], int(sys.argv[3])
times = []
for i in range(samples):
    with open(f"{repo}/hot-{i % 3}.txt", "w") as f:
        f.write(f"round {i}\n")
    time.sleep(0.03)
    start = time.perf_counter()
    subprocess.run(
        [bin_path, "--repo", repo, "checkpoint", "--kind", "post"],
        check=True, capture_output=True,
    )
    times.append((time.perf_counter() - start) * 1000)
    time.sleep(0.05)
times.sort()
print(f"{times[int((len(times) - 1) * 0.95)]:.1f}")
PY
)" || fail "latency sampling"

echo "  enqueue-ack round trip p95: ${P95}ms (budget ${BUDGET_MS}ms, $FILES files)"
python3 -c "import sys; sys.exit(0 if float('$P95') < float('$BUDGET_MS') else 1)" \
  || fail "p95 ${P95}ms exceeds ${BUDGET_MS}ms budget"

pass "latency: p95 ${P95}ms < ${BUDGET_MS}ms with a warm daemon"
