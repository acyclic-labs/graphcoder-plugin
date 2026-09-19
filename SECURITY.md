# Security Policy

## Reporting a vulnerability

Please do not open public issues for security vulnerabilities. Email **security@acyclic.dev** with a description and reproduction steps. We aim to acknowledge reports within 48 hours.

## Scope

The engine (`acyclic` CLI and daemon), the host-tool adapters (Claude Code, Codex, OpenCode), and the release pipeline (signed artifacts, SBOM, provenance).

Of particular interest: snapshot-store data exposure (secrets retained in checkpoints), guarded fork interposition bypasses, and supply-chain issues in the adapters.

## Supported versions

Pre-release: only the latest `main` is supported.
