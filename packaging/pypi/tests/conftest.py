"""A fake `acyclic` binary: logs every invocation (argv, stdin) as one JSON
line so a test can assert the exact hook sequence, and answers a few verbs
with fixed text so tool results are recognisable."""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path

import pytest

FAKE = r'''#!/usr/bin/env python3
import json, os, sys
log = os.environ["ACYCLIC_FAKE_LOG"]
argv = sys.argv[1:]
stdin = ""
if "hook" in argv:
    stdin = sys.stdin.read()
with open(log, "a") as f:
    f.write(json.dumps({"argv": argv, "stdin": stdin, "host": os.environ.get("ACYCLIC_HOST")}) + "\n")
if argv[-2:] == ["hook", "session-start"]:
    print("BRIEF: previous session left src/main.rs half migrated")
elif "timeline" in argv:
    print("#7  post  t3  write_file")
elif "rewind" in argv:
    print("rewound to #7")
sys.exit(0)
'''


@pytest.fixture
def fake_acyclic(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    binary = tmp_path / "acyclic"
    binary.write_text(FAKE)
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    log = tmp_path / "log.jsonl"
    monkeypatch.setenv("ACYCLIC_FAKE_LOG", str(log))
    monkeypatch.setenv("ACYCLIC_BIN", str(binary))
    monkeypatch.setenv("PYDANTIC_AI_NO_BANNER", "1")
    monkeypatch.delenv("ACYCLIC_DISABLED", raising=False)

    def calls() -> list[dict]:
        if not log.exists():
            return []
        rows = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        for row in rows:
            row["stdin"] = json.loads(row["stdin"]) if row["stdin"] else {}
        return rows

    return binary, calls
