# Storage-Node-Interface-Protocol

A native decentralized storage protocol for the SUM Chain
blockchain. The chain acts as a cryptographic ledger — storing
merkle roots, access lists, and fee pools — while actual file
bytes live off-chain in a libp2p peer-to-peer mesh of storage
nodes ("archives"). Archives earn Koppa by proving they hold
their deterministically-assigned chunks; the chain enforces a 3×
replication factor and slashes archives that fail to answer
Proof of Retrievability challenges in time.

No smart contracts, no IPFS dependency, no separate storage
token — storage economics settle directly at the SUM Chain
consensus layer.

---

## Platform support

Client mode (upload / download) and archive mode (long-running
operator) have different platform stories:

| Environment | Client | Archive |
|---|---|---|
| Linux | Supported | Supported |
| macOS (Apple Silicon) | Supported | Experimental |
| Windows (via WSL2) | Supported with caveats | Not supported |
| ChromeOS (via Crostini) | Supported with caveats | Not supported |

Archive operation is Linux-first. macOS Apple Silicon may join
after one operator's long-run validation completes. Windows and
ChromeOS users run SNIP as clients through their Linux-compatible
environments (WSL2 / Crostini). For the full matrix, rationale,
per-environment setup recipes, promotion criteria, and items not
planned for `v0.4.x`, see
[`docs/compatibility/platform-support.md`](docs/compatibility/platform-support.md).

---

## Install

Prebuilt binaries are published for **Linux x86_64**. Every other
supported platform builds from source — see
[`docs/getting-started/install.md`](docs/getting-started/install.md)
for both paths.

The recommended first install is the manual-verify path (download
→ check SHA256 → extract → move binaries). The curl-pipe
convenience script is:

```bash
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sh -s -- --version v0.4.0
```

The script installs `sum-node` and `e2e-helper` into
`$HOME/.local/bin` by default. It refuses to run on anything
other than Linux x86_64 and does not invoke `sudo` itself.

---

## Where to go next

- **Upload or download a file** — [`docs/getting-started/quickstart-client.md`](docs/getting-started/quickstart-client.md).
- **Stand up an archive** — [`docs/getting-started/quickstart-archive.md`](docs/getting-started/quickstart-archive.md).
- **Understand the protocol** — [`docs/protocol/overview.md`](docs/protocol/overview.md);
  then follow the story-mode walkthrough in
  [`docs/protocol/lifecycle.md`](docs/protocol/lifecycle.md).
- **Contribute code** — [`docs/architecture/crates.md`](docs/architecture/crates.md)
  and [`docs/architecture/chain-integration.md`](docs/architecture/chain-integration.md).
- **Audit security** — [`docs/security/privacy-audit.md`](docs/security/privacy-audit.md)
  and [`docs/security/threat-model.md`](docs/security/threat-model.md).
- **Track feature status** — [`docs/status/implementation-status.md`](docs/status/implementation-status.md).
- **Ship a release** — [`docs/release/release-checklist.md`](docs/release/release-checklist.md).

The full documentation entry point is
[`docs/README.md`](docs/README.md).

---

## Current-day authoritative surfaces

If two documents in this repo disagree about a fact, the one from
this list wins:

- Feature status: [`docs/status/implementation-status.md`](docs/status/implementation-status.md).
- Chain wire compatibility: [`docs/reference/chain-compat.md`](docs/reference/chain-compat.md).
- CLI commands and flags: [`docs/reference/cli.md`](docs/reference/cli.md).
- Privacy guarantees + pinning guards: [`docs/security/privacy-audit.md`](docs/security/privacy-audit.md).
- Platform support matrix: [`docs/compatibility/platform-support.md`](docs/compatibility/platform-support.md).
- Release process: [`docs/release/release-checklist.md`](docs/release/release-checklist.md).

Historical planning documents (previous phase plans, superseded
security proposals) live under [`docs/archive/`](docs/archive/README.md)
and are not authoritative.

---

## License

Dual-licensed under either of [`LICENSE-MIT`](LICENSE-MIT) or
[`LICENSE-APACHE`](LICENSE-APACHE) at your option.
