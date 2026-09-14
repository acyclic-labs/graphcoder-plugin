#!/usr/bin/env bash
# CI guard for the code-quality rules clippy and rustfmt can't express.
# Cheap, dependency-free except for the duplication check, which needs node
# (skipped with a notice when node is absent, never silently).
#
# Rules, each with the reason it exists:
#   1. Line length: rustfmt wraps code at 100 but leaves string literals and
#      comments alone, so a 200-column format string or comment sails
#      through `cargo fmt --check`. Rust: 120, except the YAML frontmatter
#      lines inside the embedded command/skill templates (`description:`),
#      which are data a host reads verbatim. Shell: 200 (acceptance prompts
#      are long by nature).
#   2. TODO format: a TODO with no owner or topic is a TODO nobody picks up.
#      Comment lines must write `TODO(topic):` / `FIXME(topic):`. Strings
#      are not checked (test fixtures legitimately contain the word).
#   3. Comment-block length: a comment longer than 30 consecutive lines is
#      usually a design note that belongs in docs/design/, or an
#      explanation of code that should instead be simpler. Split it or
#      move it. (Usage headers on scripts sit just under the cap.)
#   4. Duplication: jscpd over Rust and shell, failing when more than 3% of
#      tokens are clones (baseline when this landed: 2.4%, almost all
#      acceptance-test setup). Copy-pasted match arms and helpers were the
#      recurring review finding this guards against.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0

RUST_FILES="$(git ls-files --cached --others --exclude-standard 'crates/*.rs' 'crates/**/*.rs')"
SHELL_FILES="$(git ls-files --cached --others --exclude-standard '*.sh' 'scripts/*.sh' 'tests/**/*.sh' 'packaging/**/*.sh')"

# grep over the file list, failing closed: exit 1 (no match) is an empty
# result, anything else (unreadable file, bad pattern) fails the check
# instead of silently passing it. xargs reports a grep exit of 1 as 123.
grep_files() {
  local files="$1" pattern="$2" rc=0
  echo "$files" | xargs grep -nE "$pattern" || rc=$?
  case "$rc" in
    0 | 1 | 123) return 0 ;;
    *) echo "grep failed (exit $rc) while checking: $pattern" >&2; exit 2 ;;
  esac
}

# 1. Line length.
check_width() {
  local limit="$1" label="$2" files="$3" hits
  hits="$(grep_files "$files" ".{$((limit + 1)),}" \
    | grep -vE '^[^:]+:[0-9]+:(description|name): ' || true)"
  if [ -n "$hits" ]; then
    echo "$label lines over $limit columns (wrap the string or comment):" >&2
    echo "$hits" | cut -c1-160 >&2
    fail=1
  fi
}
check_width 120 "Rust" "$RUST_FILES"
check_width 200 "shell" "$SHELL_FILES"

# 2. TODO format, comment lines only. Two rules: any mention of TODO/FIXME
#    must carry a `TODO(topic)` somewhere on the line (opening one or
#    referring to one), and a line that *opens* one must write `TODO(topic):`
#    with the colon, so the topic and the note are visibly separate.
comment_lines="$(grep_files "$RUST_FILES $SHELL_FILES" '^[[:space:]]*(//|#)' \
  | grep -vE '^scripts/check-code-quality\.sh:' || true)"
todo_hits="$(printf '%s\n' "$comment_lines" \
  | grep -E '\b(TODO|FIXME|XXX)\b' \
  | grep -vE '\b(TODO|FIXME)\([A-Za-z0-9/ -]+\)' || true)"
opener_hits="$(printf '%s\n' "$comment_lines" \
  | grep -E '^[^:]+:[0-9]+:[[:space:]]*(//+|#+)[!/]?[[:space:]]*(TODO|FIXME)\(' \
  | grep -vE '\b(TODO|FIXME)\([A-Za-z0-9/ -]+\):' || true)"
if [ -n "$todo_hits$opener_hits" ]; then
  echo "TODOs without an owner/topic, or opened without the colon (write TODO(topic): ...):" >&2
  printf '%s\n%s\n' "$todo_hits" "$opener_hits" | grep -v '^$' | cut -c1-160 >&2
  fail=1
fi

# 3. Comment-block length.
block_hits="$(for f in $RUST_FILES $SHELL_FILES; do
  awk -v f="$f" '
    /^[[:space:]]*(\/\/|#)/ { n++; if (n == 1) start = NR; next }
    { if (n > 30) printf "%s:%d: %d consecutive comment lines\n", f, start, n; n = 0 }
    END { if (n > 30) printf "%s:%d: %d consecutive comment lines\n", f, start, n }
  ' "$f"
done)"
if [ -n "$block_hits" ]; then
  echo "comment blocks over 30 lines (move the essay to docs/design/ or simplify the code):" >&2
  echo "$block_hits" >&2
  fail=1
fi

# 4. Duplication.
if command -v npx >/dev/null 2>&1; then
  if ! npx --yes jscpd@4.3.0 --config .jscpd.json crates tests scripts packaging >/tmp/jscpd.out 2>&1; then
    echo "duplication above the 3% token threshold (see .jscpd.json):" >&2
    grep -E "Clone found|^ - |^   " /tmp/jscpd.out >&2 || tail -20 /tmp/jscpd.out >&2
    fail=1
  fi
else
  echo "note: node/npx not found, skipping the jscpd duplication check (CI runs it)" >&2
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "code quality: line length, TODO format, comment blocks, duplication all within limits"
