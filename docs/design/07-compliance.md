# Compliance

The plugin is local-first, and that *is* the compliance strategy: in v1 there is
no server-side processing of customer code, so the posture is "same trust
category as git." What remains is making that claim verifiable, and handling the
one place the engine creates new risk.

This doc was previously written as a target state in the present tense. Several
controls it described as shipped do not exist. The sections below separate what
is true today from what is intended, because a compliance doc that overstates is
worse than one that admits a gap — a security reviewer who finds one false claim
discounts the rest.

## What is true today

- **No data leaves the machine.** Stronger than previously claimed, and worth
  stating precisely: there is **no network code in the product at all**. No HTTP
  client is compiled in — no `reqwest`, `ureq`, or `hyper` in any crate — so
  there is no update check, no telemetry, and no egress to enumerate or firewall.
  Security review reduces to reviewing a local binary, not a vendor.
- **Speculation's model runs are the one exception, and they are off by
  default** — with `enabled` false (the default, and the only state a fresh
  install is ever in) nothing is sent anywhere and the claim above is literally
  true. A developer who turns them on in their own
  `~/.config/<name>/speculate.toml` is delegating to a command they named, with
  their own credentials: the egress is that command's, not a new endpoint of
  ours, and the subprocessor is whichever model vendor they already buy from.
  What bounds it is that the prompt is built from a store-computed diff —
  changed paths and the prompt excerpt, never file contents — so `exclude`
  governs it like everything else, and the child is given no filesystem route
  into the repo at all. `acyclic status` reports what has been spent. See
  [08-speculation.md](08-speculation.md).
- **Permissive license** — Apache-2.0, with dependency licence, advisory and
  source scanning enforced on every push by `cargo deny` against `deny.toml`.
- **Build provenance and an SBOM per binary** — each release binary carries a
  SLSA build-provenance attestation (`actions/attest-build-provenance`) and an
  SPDX SBOM generated from the exact `Cargo.lock` that produced it
  (`actions/attest-sbom`), and the publish job re-verifies both before packaging.
- **Security disclosure policy** — `SECURITY.md` exists and names the real threat
  model: snapshot-store data exposure, and interposition bypasses.
- **Snapshot exclusions** — `exclude` in `.acyclic/config.toml` keeps declared
  paths out of every checkpoint. This is **the compliance control that ships**,
  and its semantics matter: rules are path prefixes rather than globs, so
  `.env` does not cover `.env.example`; excluding the repo root is refused; a
  rule takes effect at the next daemon start; and generations captured before
  the rule still hold the content. A consequence worth stating plainly: for an
  excluded path, the live working tree becomes the only copy.
- **The store is always outside the repo**, under
  `~/.local/share/acyclic/stores`, never inside the working tree.
- **Adapter surface is legible** — each adapter writes a known set of files,
  enumerated per host in the README and unit-tested. It is prose and tests, not
  a machine-readable permission manifest.

## Claimed but not built

None of the following exists in the codebase. They were described here in the
present tense and are listed as gaps so nobody plans against them.

| Claim | Reality |
|---|---|
| Encryption at rest, keyed per machine via the OS keychain | No crypto anywhere in the product. The store is plain files. |
| Opt-in telemetry with a documented payload and a kill switch | No telemetry, and therefore no config key or env var to disable it. |
| Enumerable network egress, update check | No network code at all — see above. |
| Secret scanning that flags high-entropy material at checkpoint time | Does not exist. The only secret scanner is a CI guard on this repo's own commits. |
| `acyclic purge` removing content from every checkpoint | No `purge` verb exists. |
| GC physically deleting unreferenced chunks | GC is deliberately never called — see below. |
| Checkpoint TTLs and store caps in config | The only retention key is `trash_ttl_days`, which prunes rewound-away trees only. There is no checkpoint TTL and no store cap. |
| Sigstore-signed, reproducible releases | Attestations are GitHub's, not cosign/sigstore. Builds are native per target and not reproducible. macOS binaries are unsigned; notarization was declined for v1. |
| DCO sign-off | Asserted in the README, but there is no DCO section in `CONTRIBUTING.md` and no enforcing workflow. |
| Timeline exportable as an audit log | The timeline is queryable, but there is no export verb or format. |
| CVE handling and published advisories | `SECURITY.md` states pre-release support for `main` only. No CVE process exists. |

## The snapshot store is the real compliance surface

The engine deliberately checkpoints what git ignores — untracked files, `.env`,
generated artifacts. That is the differentiator, and it means the store is a
shadow copy of the repo that can retain secrets and personal data *longer than
the working tree does*.

Two things follow that were not previously stated:

- **Retention is currently forever.** With no GC and no purge, nothing ever
  leaves the store. `acyclic status` reports its size; nothing acts on it.
- **The store also retains prompts.** The metadata index persists a bounded
  excerpt of each prompt, plus paths, tool names and session ids, in SQLite
  under the store root. The old "never code, paths, or prompts" line referred to
  telemetry, but locally all three are retained, and that is a compliance
  surface in its own right.

### Blocked upstream, not merely unbuilt

Purge and GC are not backlog items; they are blocked on the pinned `acyclic-fs`
revision. A generation stays reachable only while it is a workspace head or
carries a retention fact, retention facts cannot be released, and closure proofs
do not follow generation parents. So the collector would either destroy every
checkpoint but the head or, if everything were pinned first, never free
anything. Purge has the same dependency: content cannot be removed from a
retained generation. Both need an upstream retention-release fact.

The earlier "cannot retrofit" warning — that purge-through-history needs
chunk-level tombstoning designed in from day one — was correct, and Launch 1
shipped without it. That is a known, accepted position, not an oversight, but it
should be read as a constraint rather than a plan.

## Compliance as a feature

Turn-linked history is the change-management evidence frameworks like SOC 2 and
ISO 27001 ask for, applied to agent-written code: which prompt caused which
change, and what the blast radius was.

Two honest limits on that pitch:

- Hosts without a prompt hook — Claude Desktop, VS Code, OpenCode — get
  checkpoints without turn linkage, so the headline evidence is thinner exactly
  where the adapter is thinnest.
- The "a human approved it before it reached the real tree" half rests on Safe
  Mode, which is off by default, refuses to start without a mount provider, and
  whose approval verbs are hidden CLI-only and wired into no host.

## When the cloud arrives (v2)

Certifications enter only when code starts leaving the machine: SOC 2 Type II
for the managed side, data residency options, and BYO-cloud so regulated teams
get sync and sandboxes inside their own account. The v1 store format being
dVFS-compatible means the compliance boundary moves by explicit opt-in — never
as a silent default — and the engine remains fully usable with the cloud off.

## Open questions (not yet settled)

1. ~~**Is "open source" claimable today?**~~ **Settled 2026-09-17: the repo
   goes public**, so the claim becomes true once it does — until then it is
   still false on both surfaces. The sequencing matters: the claims in the
   table above were corrected before the history becomes readable, which is the
   right order. A public repo whose compliance doc overstates is worse than a
   private one, because now anyone can check.
2. **Which of the unbuilt controls are commitments and which are deleted?**
   Encryption at rest, secret scanning, telemetry controls and audit export were
   all written as shipped. Each needs to become either a dated commitment or a
   removed claim. *Current lean: delete from marketing surfaces now, re-add when
   built.*
3. **Does the store need a cap before it needs a GC?** A cap that refuses to grow
   is implementable today; a GC is not. *Current lean: yes — an enforced cap is
   the honest interim control.*
4. **Should prompt retention be configurable?** Prompt excerpts are persisted
   unconditionally. A team excluding `.env` may be surprised that prompt text is
   kept at all. *No lean.*
5. **Does DCO get enforced or dropped?** It is asserted in the README and
   enforced nowhere. *Current lean: add the workflow — it is cheap — or stop
   claiming it.*
6. **Is `exclude` enough as the only shipped control?** It is prefix-based,
   start-time-bound, and non-retroactive, and excluded paths are invisible to
   forks and Safe Mode sessions. *No lean.*
