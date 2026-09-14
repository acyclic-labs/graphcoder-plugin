#!/usr/bin/env bash
# CI guard against committing secrets and known internal-only artifacts.
# Not a substitute for a real secret scanner, but catches the obvious and
# recurring cases cheaply, with no external dependency.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0

# Filenames that must never be tracked: local secrets-manager config,
# environment files, and common private-key extensions.
forbidden_files="$(git ls-files \
  | grep -E '(^|/)\.env(\..+)?$|(^|/)\.infisical\.json$|\.pem$|\.p12$|\.pfx$|id_rsa$|id_ed25519$' \
  || true)"
if [ -n "$forbidden_files" ]; then
  echo "forbidden files are tracked in git:" >&2
  echo "$forbidden_files" >&2
  fail=1
fi

# High-confidence live-credential shapes. Deliberately narrow (exact
# provider prefixes) to avoid flagging placeholders like "sk-..." in docs.
patterns=(
  'AKIA[0-9A-Z]{16}'                 # AWS access key ID
  'ghp_[0-9A-Za-z]{36}'              # GitHub personal access token
  'github_pat_[0-9A-Za-z_]{22,}'     # GitHub fine-grained PAT
  'sk-ant-[0-9A-Za-z-]{20,}'         # Anthropic API key
  '-----BEGIN [A-Z ]*PRIVATE KEY-----'
)
for pattern in "${patterns[@]}"; do
  set +e
  hits="$(git grep -InE -e "$pattern" -- . ':(exclude)scripts/check-no-secrets.sh' 2>&1)"
  status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    echo "possible live credential matching /$pattern/:" >&2
    echo "$hits" >&2
    fail=1
  elif [ "$status" -gt 1 ]; then
    echo "git grep failed while scanning for /$pattern/ (exit $status):" >&2
    echo "$hits" >&2
    fail=1
  fi
done

[ "$fail" -eq 0 ] && echo "no forbidden files or credential patterns found"
exit "$fail"
