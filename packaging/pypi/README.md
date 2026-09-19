# acyclic-pydantic-ai

Checkpoint every tool call of a [Pydantic AI](https://ai.pydantic.dev) agent
with [acyclic](https://acyclic.dev): rewind exactly, see the blast radius,
and pick up where the previous session left off.

```sh
npm i -g @acyclic-labs/plugin      # the engine, one binary
cd your-repo && acyclic init       # start the daemon, first snapshot
acyclic install pydantic-ai        # adds this package to your project, asks first
```

```python
from pydantic_ai import Agent
from acyclic_pydantic_ai import Acyclic

agent = Agent("anthropic:claude-opus-5", capabilities=[Acyclic()])
```

That is the whole integration. `Acyclic` is a Pydantic AI capability, so
it rides the agent's own lifecycle:

| Moment | What happens |
|---|---|
| first run | an acyclic session opens; the previous session's brief is added to the model's instructions |
| every `agent.run` | a conversation turn opens, keyed by the prompt |
| before each mutating tool | a checkpoint of the working tree, attributed to that tool call |
| after it | another checkpoint is queued |
| always | `acyclic_rewind`, `acyclic_timeline`, `acyclic_diff`, `acyclic_restore`, `acyclic_turns`, `acyclic_brief` and `acyclic_checkpoint` are tools the model can call |
| exit | the session closes and the next brief is precomputed |

Every hook is advisory. A missing binary, a stopped daemon or a slow
capture never raises into your agent, and no tool waits more than a
bounded few seconds for its checkpoint.

## Options

```python
Acyclic(
    repo=".",                       # the tree the agent edits (default: cwd)
    readonly=["search", "fetch"],   # tools that never touch the tree: no checkpoint
    mutating=None,                  # or: ONLY these tools are checkpointed
    tools=True,                     # expose rewind/timeline/... to the model
    brief=True,                     # inject the previous session's brief
    session_id=None,                # reuse one across processes
    binary=None,                    # ACYCLIC_BIN, then `acyclic` on PATH
)
```

A tool can also opt out of checkpointing with `metadata={"acyclic": "readonly"}`.
`ACYCLIC_DISABLED=1` turns the whole capability off without a code change.

## Leases

The pre-tool checkpoint records what the tool is about to touch when the
tool's arguments carry a `file_path` or `path`. Name the argument that way
and anything scheduling work alongside the agent knows which files are hot.
