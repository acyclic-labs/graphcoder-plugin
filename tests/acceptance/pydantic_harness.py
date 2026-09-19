"""The Pydantic AI agent `pydantic-e2e.sh` drives.

Three tools an agent in a repo would have (write a file, run a shell
command, search), instrumented with the `Acyclic` capability exactly the
way the README tells a customer to. Prints the acyclic session id as JSON
on its last line so the script can assert attribution and rewind by it.

`--model scripted` issues the three-step task deterministically with no
credentials, so the hook path is verified on every run; any other value is
a Pydantic AI model name (`anthropic:claude-opus-5`, ...) for a live pass.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

from pydantic_ai import Agent
from pydantic_ai.messages import ModelMessage, ModelResponse, TextPart, ToolCallPart
from pydantic_ai.models.function import AgentInfo, FunctionModel

from acyclic_pydantic_ai import Acyclic

PROMPT = (
    "Do exactly these three steps, in order, with no other file or shell operations "
    "and no commentary:\n"
    "1. Use write_file to overwrite src/main.rs with exactly the single line: MIGRATED\n"
    "2. Use run_shell to run exactly: rm .env\n"
    "3. Use run_shell to run exactly: head -c 4096 /dev/zero > generated.bin\n"
    "Then reply with the single word: done"
)

STEPS = [
    [ToolCallPart("write_file", {"file_path": "src/main.rs", "content": "MIGRATED\n"})],
    [ToolCallPart("run_shell", {"command": "rm .env"})],
    [ToolCallPart("run_shell", {"command": "head -c 4096 /dev/zero > generated.bin"})],
]


def scripted_model() -> FunctionModel:
    steps = iter(STEPS)

    def model(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        try:
            return ModelResponse(parts=list(next(steps)))
        except StopIteration:
            return ModelResponse(parts=[TextPart("done")])

    return FunctionModel(model)


def build_agent(repo: Path, model: str) -> tuple[Agent, Acyclic]:
    acyclic = Acyclic(repo=repo, readonly=["search"])
    agent = Agent(scripted_model() if model == "scripted" else model, capabilities=[acyclic])

    @agent.tool_plain
    def write_file(file_path: str, content: str) -> str:
        """Overwrite a file (relative to the repo) with content."""
        target = repo / file_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        return f"wrote {file_path}"

    @agent.tool_plain
    def run_shell(command: str) -> str:
        """Run a shell command in the repo and return its output."""
        done = subprocess.run(command, shell=True, cwd=repo, capture_output=True, text=True, timeout=60)
        return (done.stdout + done.stderr).strip() or f"exit {done.returncode}"

    @agent.tool_plain
    def search(query: str) -> str:
        """Search the repo for a string (read-only)."""
        done = subprocess.run(["grep", "-rn", query, "."], cwd=repo, capture_output=True, text=True)
        return done.stdout.strip() or "no matches"

    return agent, acyclic


async def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--model", default="scripted")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    os.chdir(repo)

    agent, acyclic = build_agent(repo, args.model)
    async with agent:
        result = await agent.run(PROMPT)
    await acyclic.aclose()
    first = result.all_messages()[0]
    print(
        json.dumps(
            {
                "session_id": acyclic.session_id,
                "output": result.output,
                # What the model was told on its first request: the previous
                # session's brief lands here when there is one.
                "instructions": getattr(first, "instructions", None) or "",
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
