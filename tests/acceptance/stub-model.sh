#!/usr/bin/env bash
# A stand-in for the model command speculation runs, so the runner can be
# tested without credentials and without spending anything.
#
# Reads the prompt on stdin exactly as a real model command would, and
# answers deterministically so a test can prove the answer came from THIS
# input. Knobs:
#   STUB_DELAY_S   sleep before answering (timeout and cancellation tests)
#   STUB_EXIT      exit code (failure tests)
#   STUB_BYTES     emit this many bytes instead of a summary (overflow test)
set -u
INPUT="$(cat)"

if [ -n "${STUB_DELAY_S:-}" ]; then
  sleep "$STUB_DELAY_S"
fi

if [ -n "${STUB_BYTES:-}" ]; then
  head -c "$STUB_BYTES" /dev/zero | tr '\0' 'x'
  exit "${STUB_EXIT:-0}"
fi

if [ "${STUB_EXIT:-0}" != "0" ]; then
  echo "stub model failed on purpose" >&2
  exit "${STUB_EXIT}"
fi

# Deterministic in the input, so a test can tell a real answer from a
# recycled one: the count of changed files the prompt listed.
FILES="$(grep -cE '^  (added|modified|removed) ' <<<"$INPUT" || true)"
echo "stub summary of $FILES file(s)"
