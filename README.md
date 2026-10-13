# graphcoder-plugin has moved

The `acyclic` CLI, daemon and MCP server now live in the Acyclic SDK
repository as [`plugin/`](https://github.com/acyclic-labs/sdk/tree/main/plugin),
where they build against `acyclic-fs` in the same workspace, are qualified by
the same CI, and release as `plugin-v<version>` tags.

- Source, docs and design history: https://github.com/acyclic-labs/sdk/tree/main/plugin
- Install: `npm i -g @acyclic-labs/plugin`, or
  `curl -fsSL https://raw.githubusercontent.com/acyclic-labs/sdk/main/plugin/scripts/install.sh | sh`
- Issues and pull requests: open them on `acyclic-labs/sdk`.

This repository is archived. Its history up to the move is preserved here;
`CHANGELOG.md` names the commit that was imported.
