from __future__ import annotations

import asyncio

import pytest
from pydantic_ai import Agent
from pydantic_ai.messages import ModelMessage, ModelResponse, TextPart, ToolCallPart
from pydantic_ai.models.function import AgentInfo, FunctionModel

from acyclic_pydantic_ai import Acyclic


def scripted(*steps: list[ToolCallPart]) -> FunctionModel:
    """A model that issues the given tool calls in order, then answers."""
    it = iter(steps)

    def model(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        try:
            return ModelResponse(parts=list(next(it)))
        except StopIteration:
            return ModelResponse(parts=[TextPart("done")])

    return FunctionModel(model)


def make_agent(cap: Acyclic, *steps: list[ToolCallPart]) -> Agent:
    agent = Agent(scripted(*steps), capabilities=[cap])

    @agent.tool_plain
    def write_file(file_path: str, content: str) -> str:
        return "written"

    @agent.tool_plain
    def search(query: str) -> str:
        return "hits"

    return agent


def hooks(calls: list[dict]) -> list[str]:
    return [c["argv"][-1] for c in calls if "hook" in c["argv"]]


async def test_full_lifecycle_fires_every_hook_in_order(fake_acyclic, tmp_path):
    _, calls = fake_acyclic
    cap = Acyclic(repo=tmp_path, readonly=["search"])
    agent = make_agent(
        cap,
        [ToolCallPart("search", {"query": "x"}), ToolCallPart("write_file", {"file_path": "src/main.rs", "content": "MIGRATED"})],
    )
    async with agent:
        result = await agent.run("migrate main")
    await cap.aclose()

    assert hooks(calls()) == ["session-start", "user-prompt", "pre-tool", "post-tool", "session-end"]
    rows = calls()
    assert all(r["host"] == "pydantic-ai" for r in rows)
    assert all(r["stdin"]["session_id"] == cap.session_id for r in rows if r["stdin"])
    assert all(r["argv"][:2] == ["--repo", str(tmp_path.resolve())] for r in rows)

    prompt = next(r for r in rows if r["argv"][-1] == "user-prompt")
    assert prompt["stdin"]["prompt"] == "migrate main"

    pre = next(r for r in rows if r["argv"][-1] == "pre-tool")
    assert pre["stdin"]["tool_name"] == "write_file"
    assert pre["stdin"]["tool_input"]["file_path"] == "src/main.rs"
    assert pre["stdin"]["tool_use_id"]

    # The previous session's brief reached the model's instructions.
    first = result.all_messages()[0]
    assert "BRIEF: previous session" in (first.instructions or "")


async def test_second_run_is_a_new_turn_in_the_same_session(fake_acyclic, tmp_path):
    _, calls = fake_acyclic
    cap = Acyclic(repo=tmp_path, tools=False)
    agent = make_agent(cap)
    async with agent:
        await agent.run("one")
        await agent.run("two")
    assert hooks(calls()) == ["session-start", "user-prompt", "user-prompt"]
    prompts = [r["stdin"]["prompt"] for r in calls() if r["argv"][-1] == "user-prompt"]
    assert prompts == ["one", "two"]


async def test_mutating_allowlist_skips_everything_else(fake_acyclic, tmp_path):
    _, calls = fake_acyclic
    cap = Acyclic(repo=tmp_path, mutating=["write_file"], brief=False, tools=False)
    agent = make_agent(cap, [ToolCallPart("search", {"query": "x"})])
    async with agent:
        await agent.run("look")
    assert hooks(calls()) == ["session-start", "user-prompt"]


async def test_native_tools_are_exposed_and_never_checkpoint_themselves(fake_acyclic, tmp_path):
    _, calls = fake_acyclic
    cap = Acyclic(repo=tmp_path, brief=False)
    agent = make_agent(cap, [ToolCallPart("acyclic_timeline", {"limit": 5})], [ToolCallPart("acyclic_rewind", {"checkpoint": 7})])
    async with agent:
        result = await agent.run("undo")
    returns = [p.content for m in result.all_messages() for p in m.parts if p.part_kind == "tool-return"]
    assert returns == ["#7  post  t3  write_file", "rewound to #7"]
    assert hooks(calls()) == ["session-start", "user-prompt"]
    argv = [c["argv"] for c in calls() if "hook" not in c["argv"]]
    assert argv[0][2:] == ["timeline", "--session", cap.session_id, "--limit", "5"]
    assert argv[1][2:] == ["rewind", "7", "--yes"]


async def test_missing_binary_is_a_silent_no_op(tmp_path, monkeypatch):
    monkeypatch.setenv("ACYCLIC_BIN", str(tmp_path / "nope"))
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.setenv("PYDANTIC_AI_NO_BANNER", "1")
    cap = Acyclic(repo=tmp_path)
    assert not cap.enabled
    agent = make_agent(cap, [ToolCallPart("write_file", {"file_path": "a", "content": "b"})])
    async with agent:
        result = await agent.run("go")
    assert result.output == "done"
    assert "acyclic_rewind" not in [p.tool_name for m in result.all_messages() for p in m.parts if hasattr(p, "tool_name")]


async def test_disabled_env_var_turns_it_off(fake_acyclic, tmp_path, monkeypatch):
    _, calls = fake_acyclic
    monkeypatch.setenv("ACYCLIC_DISABLED", "1")
    cap = Acyclic(repo=tmp_path)
    agent = make_agent(cap, [ToolCallPart("write_file", {"file_path": "a", "content": "b"})])
    async with agent:
        await agent.run("go")
    assert calls() == []


async def test_a_wedged_hook_is_killed_and_the_tool_still_runs(fake_acyclic, tmp_path, monkeypatch):
    binary, _ = fake_acyclic
    binary.write_text("#!/bin/sh\nsleep 30\n")
    monkeypatch.setattr("acyclic_pydantic_ai._PRE_TOOL_TIMEOUT", 0.2)
    monkeypatch.setattr("acyclic_pydantic_ai._HOOK_TIMEOUT", 0.2)
    monkeypatch.setattr("acyclic_pydantic_ai._SESSION_START_TIMEOUT", 0.2)
    cap = Acyclic(repo=tmp_path, tools=False, brief=False)
    agent = make_agent(cap, [ToolCallPart("write_file", {"file_path": "a", "content": "b"})])
    async with agent:
        result = await asyncio.wait_for(agent.run("go"), 5)
    assert result.output == "done"
