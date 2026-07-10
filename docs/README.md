# SNIP documentation

Storage-Node-Interface-Protocol (SNIP) is a client of the SUM Chain
that stores files off-chain in a libp2p mesh and pins their integrity
and access rules on chain. This directory is the entry point for
everything the SNIP team documents.

The [root README](../README.md) is a concise landing page. This
directory is where the material lives once you want depth.

## Where to start

- **New to SNIP as an end user?** Read
  [`getting-started/quickstart-client.md`](getting-started/quickstart-client.md),
  then dip into [`client/upload-and-download.md`](client/upload-and-download.md).
- **Standing up an archive?** Read
  [`getting-started/quickstart-archive.md`](getting-started/quickstart-archive.md),
  then [`operator/mainnet-bringup.md`](operator/mainnet-bringup.md).
- **Understanding the protocol?** Read
  [`protocol/overview.md`](protocol/overview.md) then
  [`protocol/lifecycle.md`](protocol/lifecycle.md).
- **Contributing code?** Read
  [`architecture/crates.md`](architecture/crates.md) and
  [`architecture/chain-integration.md`](architecture/chain-integration.md).
- **Auditing security?** Read
  [`security/privacy-audit.md`](security/privacy-audit.md) and
  [`security/threat-model.md`](security/threat-model.md).
- **Shipping a release?** Read
  [`release/release-checklist.md`](release/release-checklist.md).

## Directory map

| Directory | Contents |
|---|---|
| [`getting-started/`](getting-started/) | Install + fastest paths to a first upload and a first archive |
| [`protocol/`](protocol/) | Protocol overview, lifecycle walkthrough, V2 state machine, Proof of Retrievability, diagrams |
| [`architecture/`](architecture/) | Crate map, chain integration, networking |
| [`operator/`](operator/) | Runbook, mainnet bring-up, monitoring |
| [`client/`](client/) | End-user upload / download, key management |
| [`reference/`](reference/) | Canonical CLI, chain compatibility, RPC methods, config flags |
| [`security/`](security/) | Privacy audit, threat model |
| [`compatibility/`](compatibility/) | Platform support matrix |
| [`release/`](release/) | Release checklist, versioning |
| [`status/`](status/) | Current implementation-status matrix |
| [`roadmap/`](roadmap/) | Forward-looking, planned but not implemented |
| [`archive/`](archive/) | Historical planning documents, non-normative |

Historical planning documents (previous phase plans, superseded
security recommendations, etc.) live in
[`archive/`](archive/README.md). Everything else is normative:
statements in a non-archive file are the current-day source of
truth.

## Current-day authoritative surfaces

If two documents in this repo disagree about a fact, the one from
this list wins:

- Feature status: [`status/implementation-status.md`](status/implementation-status.md).
- Chain wire compatibility: [`reference/chain-compat.md`](reference/chain-compat.md).
- CLI commands and flags: [`reference/cli.md`](reference/cli.md).
- Privacy guarantees + pinning guards: [`security/privacy-audit.md`](security/privacy-audit.md).
- Platform support matrix: [`compatibility/platform-support.md`](compatibility/platform-support.md).
- Release process: [`release/release-checklist.md`](release/release-checklist.md).

## Documentation drift prevention

Two scripts run inside `make release-check`:

- `scripts/check-docs-links.sh` — every relative Markdown link
  inside `docs/**/*.md`, `README.md`, and `CHANGELOG.md` must
  resolve to an existing file.
- `scripts/check-cli-doc.sh` — every clap flag on `sum-node` and
  `e2e-helper` must have a row under its subcommand's section in
  [`reference/cli.md`](reference/cli.md).

If either fails, CI blocks the merge. See
[`release/release-checklist.md`](release/release-checklist.md) for
how these interact with the release gate.
