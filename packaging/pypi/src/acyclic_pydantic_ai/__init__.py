"""acyclic for Pydantic AI: a capability that checkpoints every tool call.

    from pydantic_ai import Agent
    from acyclic_pydantic_ai import Acyclic

    agent = Agent("anthropic:claude-opus-5", capabilities=[Acyclic()])

That one line gives a Pydantic AI agent what the Claude Code adapter gives
Claude Code: a checkpoint before and after every mutating tool call, each
one attributed to the conversation turn (the `agent.run` prompt) that caused
it, the previous session's brief injected into the model's instructions,
and rewind/timeline/diff/restore as native tools the model can call.

Everything runs through the `acyclic` binary on PATH; the daemon does the
work. Every hook is advisory: a missing binary, a stopped daemon or a slow
capture never raises into the agent and never blocks a tool for more than a
bounded wait. Turn it off with `ACYCLIC_DISABLED=1`.
"""

from __future__ import annotations

import asyncio
import atexit
import json
import os
import shutil
import subprocess
import sys
import uuid
import warnings
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from pydantic_ai import FunctionToolset, RunContext
from pydantic_ai.capabilities import AbstractCapability

__all__ = ["Acyclic", "__version__"]
__version__ = "0.0.1"

#: What the daemon records as the host of every session this package opens.
HOST = "pydantic-ai"

# Bounds on how long a hook may hold the agent. The binary bounds itself
# (a pre-tool wait is capped at 2 s inside `acyclic hook`), so these only
# catch a wedged process; on expiry the hook is killed and the agent
# continues without its checkpoint.
_PRE_TOOL_TIMEOUT = 5.0
_HOOK_TIMEOUT = 10.0
_SESSION_START_TIMEOUT = 15.0
_CLI_TIMEOUT = 120.0

# Tool arg keys the binary reads for the lease (what the tool is about to
# touch). Anything else records a wildcard, which is still correct.
_PATH_KEYS = ("file_path", "path")


def _find_binary(explicit: str | os.PathLike[str] | None) -> str | None:
    """An explicit path or ACYCLIC_BIN must resolve to an executable; a bare
    name is looked up on PATH. Anything else is "no binary", never a path
    that fails on first use."""
    candidate = os.fspath(explicit) if explicit else os.environ.get("ACYCLIC_BIN") or "acyclic"
    return shutil.which(candidate)


class Acyclic(AbstractCapability[Any]):
    """Checkpoint every mutating tool call of the agent this is attached to.

    Args:
        repo: The repository the agent edits. Defaults to the current
            working directory; the daemon must have been started there with
            ``acyclic init``.
        readonly: Tool names that never change the tree (a search, a
            fetch). They run without a checkpoint on either side. A tool
            whose ``metadata`` carries ``{"acyclic": "readonly"}`` is
            treated the same way.
        mutating: If given, ONLY these tools are checkpointed and every
            other tool is treated as read-only. Use this when most of the
            agent's tools are lookups.
        tools: Expose rewind, timeline, diff, restore, turns, brief and
            checkpoint to the model as native tools. On by default: this
            is how the model undoes its own last step.
        brief: Inject the previous session's brief into the instructions of
            the first request. On by default.
        session_id: Reuse a session id (for example across processes that
            serve one long conversation). Defaults to a fresh one per
            ``Acyclic`` instance, which is one per process in the common
            case.
        binary: Path to the ``acyclic`` binary. Defaults to ``ACYCLIC_BIN``
            in the environment, then ``acyclic`` on PATH.
    """

    def __init__(
        self,
        *,
        repo: str | os.PathLike[str] | None = None,
        readonly: Iterable[str] = (),
        mutating: Iterable[str] | None = None,
        tools: bool = True,
        brief: bool = True,
        session_id: str | None = None,
        binary: str | os.PathLike[str] | None = None,
    ) -> None:
        self.repo = Path(repo).resolve() if repo else Path.cwd()
        self.readonly = frozenset(readonly)
        self.mutating = frozenset(mutating) if mutating is not None else None
        self.expose_tools = tools
        self.want_brief = brief
        self.session_id = session_id or str(uuid.uuid4())
        self.binary = _find_binary(binary)
        self.enabled = self.binary is not None and not os.environ.get("ACYCLIC_DISABLED")
        if self.binary is None and not os.environ.get("ACYCLIC_DISABLED"):
            warnings.warn(
                "acyclic: no `acyclic` binary on PATH (or ACYCLIC_BIN); checkpointing is off. "
                "Install it with `npm i -g @acyclic-labs/plugin`.",
                stacklevel=2,
            )
        self._brief: str = ""
        self._session_started = False
        self._session_ended = False
        self._session_lock: asyncio.Lock | None = None
        self._own_tools: frozenset[str] = frozenset()
        if self.enabled:
            atexit.register(self._end_session_sync)

    # -- lifecycle -----------------------------------------------------------

    def get_instructions(self) -> Callable[[], Any] | None:
        if not (self.enabled and self.want_brief):
            return None

        async def previous_session_brief() -> str | None:
            await self._ensure_session()
            return self._brief or None

        return previous_session_brief

    async def before_run(self, ctx: RunContext[Any]) -> None:
        if not self.enabled:
            return
        await self._ensure_session()
        await self._hook("user-prompt", {"prompt": _prompt_text(ctx.prompt)}, _HOOK_TIMEOUT)

    async def wrap_tool_execute(self, ctx: RunContext[Any], *, call: Any, tool_def: Any, args: Any, handler: Any) -> Any:
        if not (self.enabled and self._is_mutating(tool_def)):
            return await handler(args)
        tool_input = _tool_input(args)
        base = {"tool_name": tool_def.name, "tool_use_id": call.tool_call_id}
        await self._hook("pre-tool", {**base, "tool_input": tool_input}, _PRE_TOOL_TIMEOUT)
        try:
            return await handler(args)
        finally:
            # Enqueue only; the daemon captures after this returns.
            await self._hook("post-tool", base, _HOOK_TIMEOUT)

    def get_toolset(self) -> FunctionToolset[Any] | None:
        if not (self.enabled and self.expose_tools):
            return None
        toolset = self._build_toolset()
        self._own_tools = frozenset(toolset.tools)
        return toolset

    async def aclose(self) -> None:
        """End the acyclic session now instead of at interpreter exit."""
        if self.enabled and self._session_started and not self._session_ended:
            self._session_ended = True
            await self._hook("session-end", {}, _HOOK_TIMEOUT)

    # -- hooks ---------------------------------------------------------------

    async def _ensure_session(self) -> None:
        if self._session_started:
            return
        if self._session_lock is None:
            self._session_lock = asyncio.Lock()
        async with self._session_lock:
            if self._session_started:
                return
            self._session_started = True
            # Stdout is the previous session's brief, exactly what the
            # SessionStart hook prints into Claude Code's context.
            out = await self._hook("session-start", {"source": "startup"}, _SESSION_START_TIMEOUT)
            self._brief = out.strip()

    def _payload(self, extra: dict[str, Any]) -> bytes:
        return json.dumps({"session_id": self.session_id, **extra}, default=str).encode()

    def _hook_argv(self, event: str) -> list[str]:
        assert self.binary is not None
        return [self.binary, "--repo", str(self.repo), "hook", event]

    def _hook_env(self) -> dict[str, str]:
        return {**os.environ, "ACYCLIC_HOST": HOST}

    async def _hook(self, event: str, extra: dict[str, Any], timeout: float) -> str:
        """Run one hook. Never raises: a hook may not break a tool call."""
        try:
            proc = await asyncio.create_subprocess_exec(
                *self._hook_argv(event),
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
                env=self._hook_env(),
            )
        except (OSError, ValueError):
            return ""
        try:
            out, _ = await asyncio.wait_for(proc.communicate(self._payload(extra)), timeout)
        except asyncio.TimeoutError:
            proc.kill()
            return ""
        except (OSError, ValueError):
            return ""
        return out.decode(errors="replace")

    def _end_session_sync(self) -> None:
        if not self._session_started or self._session_ended:
            return
        self._session_ended = True
        try:
            subprocess.run(
                self._hook_argv("session-end"),
                input=self._payload({}),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=self._hook_env(),
                timeout=_HOOK_TIMEOUT,
                check=False,
            )
        except (OSError, ValueError, subprocess.SubprocessError):
            pass

    def _is_mutating(self, tool_def: Any) -> bool:
        name = tool_def.name
        if name in self._own_tools:
            return False
        meta = getattr(tool_def, "metadata", None) or {}
        if isinstance(meta, dict) and meta.get("acyclic") == "readonly":
            return False
        if self.mutating is not None:
            return name in self.mutating
        return name not in self.readonly

    # -- native tools --------------------------------------------------------

    async def _cli(self, *args: str) -> str:
        assert self.binary is not None
        try:
            proc = await asyncio.create_subprocess_exec(
                self.binary,
                "--repo",
                str(self.repo),
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
                env=self._hook_env(),
            )
            out, _ = await asyncio.wait_for(proc.communicate(), _CLI_TIMEOUT)
        except asyncio.TimeoutError:
            return f"acyclic {args[0]}: timed out"
        except (OSError, ValueError) as error:
            return f"acyclic {args[0]}: {error}"
        text = out.decode(errors="replace").strip()
        return text or f"acyclic {args[0]}: done"

    def _build_toolset(self) -> FunctionToolset[Any]:
        toolset: FunctionToolset[Any] = FunctionToolset(id="acyclic")
        session = self.session_id

        @toolset.tool_plain
        async def acyclic_checkpoint(message: str) -> str:
            """Snapshot the working tree right now and return its checkpoint id.
            Do this before a risky change so you can rewind to it."""
            return await self._cli("checkpoint", "-m", message, "--session-id", session)

        @toolset.tool_plain
        async def acyclic_timeline(limit: int = 20) -> str:
            """Recent checkpoints, newest first, with the conversation turn that caused each."""
            return await self._cli("timeline", "--session", session, "--limit", str(limit))

        @toolset.tool_plain
        async def acyclic_rewind(checkpoint: int) -> str:
            """Restore the whole working tree exactly as it was at a checkpoint
            (untracked and gitignored files included). Use this instead of
            hand-reverting a failed attempt."""
            return await self._cli("rewind", str(checkpoint), "--yes")

        @toolset.tool_plain
        async def acyclic_rewind_to_session_start() -> str:
            """Undo everything this session did: restore the tree as it was
            before the first checkpoint of this session."""
            return await self._cli("rewind", "--session-start", session, "--yes")

        @toolset.tool_plain
        async def acyclic_diff(from_checkpoint: int | None = None, to_checkpoint: int | None = None) -> str:
            """What changed between two checkpoints (default: everything this session changed)."""
            args = ["diff"]
            if from_checkpoint is not None:
                args.append(str(from_checkpoint))
            if to_checkpoint is not None:
                args.append(str(to_checkpoint))
            return await self._cli(*args)

        @toolset.tool_plain
        async def acyclic_restore(checkpoint: int, path: str) -> str:
            """Bring back ONE file as it was at a checkpoint, leaving everything else."""
            return await self._cli("restore", str(checkpoint), path)

        @toolset.tool_plain
        async def acyclic_turns(limit: int = 20) -> str:
            """Conversation turns of this session: which prompt caused which checkpoints."""
            return await self._cli("turns", "--session", session, "--limit", str(limit))

        @toolset.tool_plain
        async def acyclic_brief() -> str:
            """Where the previous session ended: its last state and abandoned branches."""
            return await self._cli("brief", "--current", session)

        return toolset


def _prompt_text(prompt: Any) -> str:
    if prompt is None:
        return ""
    if isinstance(prompt, str):
        return prompt
    if isinstance(prompt, (list, tuple)):
        return "\n".join(p if isinstance(p, str) else str(p) for p in prompt)
    return str(prompt)


def _tool_input(args: Any) -> dict[str, Any]:
    """The subset of the tool's arguments the hook understands, plus the rest
    for anyone reading the payload; the binary ignores unknown keys."""
    if not isinstance(args, dict):
        return {}
    out: dict[str, Any] = {}
    for key, value in args.items():
        if key in _PATH_KEYS and isinstance(value, (str, os.PathLike)):
            out[key] = os.fspath(value)
        elif isinstance(value, (str, int, float, bool)) or value is None:
            out[key] = value
    return out


if sys.version_info < (3, 10):  # pragma: no cover
    raise ImportError("acyclic-pydantic-ai needs Python 3.10 or newer")
