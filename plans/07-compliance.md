# Compliance

The plugin is open source and local-first, and that combination *is* the compliance strategy: in v1 there is no server-side processing of customer code, so the posture is "same trust category as git." What remains is making that claim verifiable, and handling the one place the engine creates new risk.

## Local-first as the baseline

- **No data leaves the machine** — no DPA to sign, no data-residency question, no subprocessor list. Security review reduces to reviewing a local binary, not a vendor.
- **Telemetry is opt-in and documented** — exactly what is sent (never code, paths, or prompts) is enumerated in the docs and inspectable in the source; a single config key and an env var disable it.
- **Network egress is enumerable** — the daemon's only outbound calls (update check, opt-in telemetry) are listed and firewall-blockable without breaking the product.

## Open source made verifiable

- **Permissive license** (Apache-2.0) with DCO sign-off; dependency license scanning in CI so the SBOM stays clean.
- **Signed, reproducible releases** — sigstore-signed binaries with SLSA build provenance and a published SBOM per release. An open repo without signed artifacts is no better than closed source to a security team.
- **Security disclosure policy** — SECURITY.md, a private reporting channel, CVE handling, published advisories. Host-tool plugins are a supply-chain target; act like one.
- **Adapter permissions are minimal and legible** — each host adapter declares exactly which hooks and commands it registers, so what the plugin can see and do inside Claude Code or Codex is auditable per host.

## The snapshot store is the real compliance surface

The engine deliberately checkpoints what git ignores — untracked files, `.env`, generated artifacts. That is the differentiator, and it means the store is a shadow copy of the repo that can retain secrets and personal data *longer than the working tree does*. It gets first-class controls:

- **Encryption at rest** for the object store, keyed per machine (OS keychain).
- **Snapshot exclusions and secret scanning** — guarded-path-style rules keep declared secret paths out of checkpoints entirely; a scanner flags high-entropy material at checkpoint time.
- **Real deletion** — `acyclic purge` removes content from every checkpoint that references it (GDPR erasure must reach history), and GC physically deletes unreferenced chunks rather than orphaning them.
- **Team-enforced retention** — checkpoint TTLs and store caps in the checked-in `.acyclic/config.toml`, so policy ships with the repo rather than depending on each dev's defaults.

**Engineering consequence (Launch 1, cannot retrofit):** purge-through-history and exclusions must be designed into the Merkle store from the start. In a naive content-addressed DAG, "delete this from all checkpoints" changes every ancestor hash. The store needs chunk-level tombstoning (or an indirection layer between logical hashes and physical chunks) from day one.

## Compliance as a feature

Turn-linked history is precisely the change-management evidence frameworks like SOC 2 and ISO 27001 ask for, applied to agent-written code: which prompt caused which change, what the full blast radius was, and that a human reviewed it before it reached the real tree (Safe Mode's approval gate). Teams adopting coding agents currently have no answer to "who authorized this change?" — the timeline *is* the answer, exportable as an audit log.

## When the cloud arrives (v2)

Certifications enter only when code starts leaving the machine: SOC 2 Type II for the managed side, data residency options, and BYO-cloud so regulated teams get dVFS sync and sandboxes inside their own account. The v1 store format being dVFS-compatible means the compliance boundary moves by explicit opt-in — never as a silent default — and the open-source engine remains fully usable with the cloud switched off.
