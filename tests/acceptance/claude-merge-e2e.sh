#!/usr/bin/env bash
# Merge v2, live: real Claude Code sessions driving the fork-decompose skill
# against the merge primitive. Two scenarios, each in its own isolated repo:
#
#   S1 PARTITION round. Three functions in ONE file plus three bullets in
#      ONE README, asked for via /fork. The agent forks, dispatches, and
#      promotes every passing fork. Later promotes must merge (the first
#      swaps); the tree must end with all three implemented, tests green,
#      no markers, no live forks, and merge rows in the timeline.
#
#   S2 Conflict resolution. Two forks edit the same line, seeded through the
#      CLI so the conflict is deterministic. The second promote rebases the
#      fork with diff3 markers. The agent is then asked to resolve and
#      promote it. The landed file must carry both changes and no markers.
#
# Needs the `claude` CLI and credentials; skips (exit 0) when absent unless
# ACYCLIC_E2E_REQUIRED=1. Costs two short model sessions.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

skip() {
  if [ "${ACYCLIC_E2E_REQUIRED:-0}" = "1" ]; then
    fail "$*"
  fi
  echo "SKIP($(basename "$0")): $*"
  exit 0
}

command -v claude >/dev/null 2>&1 || skip "claude CLI not on PATH"
BIN_DIR="$(cd "$(dirname "$BIN")" && pwd)"

seed_project() {
  mkdir -p "$R/src" "$R/tests" "$R/.acyclic"
  printf 'store_dir = "%s"\n' "$STORES" > "$R/.acyclic/config.toml"
  cat > "$R/src/textkit.py" <<'EOF'
"""textkit: tiny text utilities."""


def slugify(text):
    """Return a URL slug for text."""
    raise NotImplementedError("slugify")


def word_count(text):
    """Return the number of words in text."""
    raise NotImplementedError("word_count")


def truncate(text, limit):
    """Return text cut to limit characters with an ellipsis."""
    raise NotImplementedError("truncate")
EOF
  cat > "$R/tests/test_textkit.py" <<'EOF'
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))
from textkit import slugify, word_count, truncate  # noqa: E402


class TextkitTests(unittest.TestCase):
    def test_slugify(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")
        self.assertEqual(slugify("  Many   spaces here "), "many-spaces-here")

    def test_word_count(self):
        self.assertEqual(word_count("one two  three"), 3)
        self.assertEqual(word_count(""), 0)

    def test_truncate(self):
        self.assertEqual(truncate("abcdefgh", 5), "abcd…")
        self.assertEqual(truncate("abc", 5), "abc")
EOF
  cat > "$R/README.md" <<'EOF'
# textkit

Tiny text utilities.

## Functions

- slugify: TODO
- word_count: TODO
- truncate: TODO
EOF
  printf '__pycache__/\n' > "$R/.gitignore"
  (cd "$R" && git init -q && git add -A && git commit -qm scaffold)
}

run_claude() {
  # $1: prompt. Prints the session id; the transcript lands in $WORK.
  local prompt="$1" out
  out="$(cd "$R" && PATH="$BIN_DIR:$PATH" claude -p "$prompt" \
    ${CLAUDE_MODEL_ARGS[@]+"${CLAUDE_MODEL_ARGS[@]}"} \
    --settings .claude/settings.json \
    --dangerously-skip-permissions \
    --output-format json 2>"$WORK/claude.stderr")" \
    || skip "claude session failed: $(tail -c 300 "$WORK/claude.stderr")"
  printf '%s' "$out" > "$WORK/claude.json"
  printf '%s' "$out" | grep -o '"session_id"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 | sed 's/.*"\([^"]*\)"$/\1/'
}

no_markers_in_tree() {
  ! grep -rl '^<<<<<<< ' "$1" --include='*.py' --include='*.md' 2>/dev/null | grep -q .
}

# ---------------------------------------------------------------------------
# S1: a live /fork PARTITION round over a shared file
# ---------------------------------------------------------------------------
seed_project
acy init >/dev/null || fail "S1: init"
acy install claude-code >/dev/null || fail "S1: install"

S1_PROMPT='/fork fan_out=3 test_command="python3 -m unittest discover -s tests" '
S1_PROMPT+='Implement slugify, word_count, and truncate in src/textkit.py so tests/test_textkit.py passes, '
S1_PROMPT+='and replace each function'"'"'s TODO bullet in README.md with a one-line description of what it does. '
S1_PROMPT+='Treat the three functions as independent parts.'
SID="$(run_claude "$S1_PROMPT")"
[ -n "$SID" ] || fail "S1: no session id"

# The work landed on the real tree: every function implemented, tests green,
# every README bullet rewritten, no conflict markers anywhere.
(cd "$R" && python3 -m unittest discover -s tests >/dev/null 2>&1) || fail "S1: tests do not pass on the landed tree: $(cd "$R" && python3 -m unittest discover -s tests 2>&1 | tail -5)"
! grep -q 'NotImplementedError' "$R/src/textkit.py" || fail "S1: a function is still unimplemented"
! grep -q 'TODO' "$R/README.md" || fail "S1: README still has TODO bullets: $(cat "$R/README.md")"
no_markers_in_tree "$R" || fail "S1: conflict markers left in the tree"
acy forks | grep -q 'no live forks' || fail "S1: forks left live: $(acy forks)"

# The round went through the merge primitive: forks were cut, promoted, and
# at least one later promote landed onto a moved mainline.
TL="$(acy timeline --limit 200)"
echo "$TL" | grep -q 'fork base' || fail "S1: no fork was cut: $TL"
echo "$TL" | grep -q 'promote fork' || fail "S1: no promote row: $TL"
echo "$TL" | grep -Eq 'onto moved mainline' || fail "S1: no merge/replay onto a moved mainline (the round did not exercise the merge): $TL"
# If the agent's parts touched the same file (they should: one file, three
# functions), a content merge happened. Record which it was for the report.
if echo "$TL" | grep -Eq 'merged [1-9][0-9]* file'; then
  S1_MODE="content merge"
else
  S1_MODE="path replay only"
fi

acy stop >/dev/null 2>&1 || true
rm -rf "$R" "$STORES"

# ---------------------------------------------------------------------------
# S2: the agent resolves a real conflict the engine wrote into a fork
# ---------------------------------------------------------------------------
mkdir -p "$STORES"
seed_project
acy init >/dev/null || fail "S2: init"
acy install claude-code >/dev/null || fail "S2: install"

fork() { acy fork -n 1 | awk '/^fork /{print $2; exit}'; }
fork_path() { acy forks | awk -v f="$1" '$1==f{print $5}'; }
A="$(fork)"; B="$(fork)"
# Both forks rewrite the module docstring line: a guaranteed same-line conflict.
sed -e 's/^"""textkit: tiny text utilities."""$/"""textkit: tiny text utilities (slug support)."""/' "$R/src/textkit.py" > "$(fork_path "$A")/src/textkit.py"
sed -e 's/^"""textkit: tiny text utilities."""$/"""textkit: tiny text utilities (count support)."""/' "$R/src/textkit.py" > "$(fork_path "$B")/src/textkit.py"
printf 'from B\n' > "$(fork_path "$B")/b-note.txt"
acy promote "$A" >/dev/null || fail "S2: promote A"
if OUT="$(acy promote "$B" 2>&1)"; then fail "S2: B should conflict: $OUT"; fi
echo "$OUT" | grep -q 'src/textkit.py: 1 conflicting hunk(s)' || fail "S2: conflict shape: $OUT"
BP="$(fork_path "$B")"
grep -q '^<<<<<<< fork ' "$BP/src/textkit.py" || fail "S2: markers not in the fork"

S2_PROMPT="Fork $B of this repository lives at $BP. Its last \`acyclic promote $B\` reported a merge conflict "
S2_PROMPT+="and wrote diff3 conflict markers into $BP/src/textkit.py. Resolve that conflict in the fork so the module "
S2_PROMPT+="docstring reads exactly: \"\"\"textkit: tiny text utilities (slug support, count support).\"\"\" "
S2_PROMPT+="and no <<<<<<<, |||||||, ======= or >>>>>>> lines remain. Edit only that file inside the fork directory, "
S2_PROMPT+="do not touch the real repository files, then run exactly: acyclic promote $B. "
S2_PROMPT+="Report the promote command's output verbatim."
SID2="$(run_claude "$S2_PROMPT")"
[ -n "$SID2" ] || fail "S2: no session id"

[ "$(head -1 "$R/src/textkit.py")" = '"""textkit: tiny text utilities (slug support, count support)."""' ] || fail "S2: resolution did not land: $(head -3 "$R/src/textkit.py")"
no_markers_in_tree "$R" || fail "S2: markers in the real tree"
[ "$(cat "$R/b-note.txt")" = "from B" ] || fail "S2: the fork's other file did not land with the resolution"
acy forks | grep -q 'no live forks' || fail "S2: fork not consumed: $(acy forks)"
grep -q "promoted" "$WORK/claude.json" || fail "S2: agent did not report the promote output: $(tail -c 600 "$WORK/claude.json")"

pass "claude-merge-e2e: live /fork partition round landed three parts through the merge primitive ($S1_MODE); agent resolved an engine-written conflict in a fork and promoted it"
