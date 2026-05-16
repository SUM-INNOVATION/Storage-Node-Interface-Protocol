# SNIP platform support

This document is the source of truth for which operating systems
and environments SNIP supports, in which roles, and at what
maturity level. The compact summary in [`README.md`](../README.md)
points back here for the full rationale and per-environment setup
recipes.

Two operator roles are exercised through the same `sum-node`
binary with different flags:

1. **Client / user.** Stores and retrieves files by paying storage
   and transaction fees. Does **not** register as an archive node,
   does **not** stake, does **not** run `listen`. Cannot earn
   rewards; cannot be slashed. Sub-commands: `ingest-v2`,
   `download`, `share`, `revoke`, `update-access`,
   `register-encryption-key`, `resume`, `abandon`.
2. **Archive / operator.** Registers as `ArchiveNode` on chain,
   stakes `1_000_000_000` base units (1 Koppa), runs a
   long-running `listen` process, stores chunks on behalf of
   clients, can earn rewards, can be slashed for protocol
   violations.

## Support matrix

| Environment | Client mode | Archive mode |
|---|---|---|
| Linux x86_64 | ✅ Supported | ✅ Supported |
| Linux aarch64 | ✅ Supported | ⚠️ Supported with caveats |
| macOS (Apple Silicon) | ✅ Supported | ⚠️ Experimental |
| macOS (Intel) | Skip unless requested | Skip unless requested |
| Windows native | ⚠️ Experimental / not recommended | ❌ Not supported |
| Windows + WSL2 | ⚠️ Supported with caveats | ❌ Not supported |
| ChromeOS Crostini | ⚠️ Supported with caveats | ❌ Not supported |
| ChromeOS native | ❌ Not supported | ❌ Not supported |

### How to read the cells

- **✅ Supported.** CI builds + workspace tests pass on this
  environment AND at least one operator has exercised the path
  end-to-end against mainnet or the local mirror. Documented
  setup snippets exist in this file. No surprises expected for
  conformant operator setups.
- **⚠️ Supported with caveats.** Works for the documented use
  case. Operator-visible quirks are listed below. No SLA implied.
- **⚠️ Experimental.** Builds and likely works, but no end-to-end
  validation has been recorded. Use at your own risk; report
  outcomes so we can promote (or demote) the cell.
- **❌ Not supported.** Explicit non-goal for `v0.4.x`. Either
  architecturally blocked (e.g. inbound networking gated by the
  host OS) or out of scope until operator demand surfaces.

All four environments — Linux, macOS, Windows, and ChromeOS — have
a documented client path. Windows native and ChromeOS native are
explicitly not supported themselves; their documented client paths
are WSL2 and Crostini respectively.

## Per-environment setup

### Linux x86_64 / aarch64 (primary)

Validated on mainnet (`v0.4.0-rc3` bring-up: three archive nodes
on Hetzner CX21 VPSs, first Public V2 round-trip byte-identical).

**Linux x86_64 has a prebuilt tarball.** This is the only cell
in the matrix with prebuilts in the `v0.4.x` line. See
[`INSTALL.md`](INSTALL.md) for both the manual-verify path
(download, check SHA256, extract, move binaries) and the
curl-pipe convenience script. The build-from-source path below
remains supported and is the contract on every other cell.

```bash
# Build essentials.
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl git
# Rust toolchain (rustup reads rust-toolchain.toml).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"

# Clone + build at the current release-candidate tag.
git clone https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol.git
cd Storage-Node-Interface-Protocol
git checkout v0.4.0-rc4            # or <latest-release-candidate-tag>
make release-check
```

aarch64 is the same toolchain but has **no prebuilt tarball** in
`v0.4.x` — build from source. Promotion of archive mode on
aarch64 from "with caveats" to "supported" requires one operator
to run a long-lived archive against mainnet for the documented
validation window. A prebuilt aarch64 tarball is a candidate for
`v0.4.1+` once that validation lands.

For the full mainnet archive bring-up flow see
[`MAINNET-BRINGUP.md`](MAINNET-BRINGUP.md).

### macOS (Apple Silicon)

Client mode is expected to work cleanly: cargo builds against the
`aarch64-apple-darwin` triple, the rustls-based TLS stack and
libp2p QUIC have no macOS-specific gotchas. CI build coverage is
a follow-up PR.

Archive mode on macOS is **experimental.** Known operator-visible
concerns:

- macOS Application Firewall prompts on first launch when
  `sum-node listen` opens its QUIC socket. Pre-approve via
  `socketfilterfw --add /path/to/sum-node` (requires admin) if
  running under launchd where the modal would be invisible.
- Lid-close / battery / sleep behavior is operator-controlled.
  Production archive operation on a laptop is not a sensible
  posture; use a Mac mini or dedicated host if you go this route.
- launchd plist templates are not in-tree yet (deferred).

Intel mac (`x86_64-apple-darwin`) is skipped from the default
matrix; add a CI lane on request.

### Windows — recommended path is WSL2

For `v0.4.x` the supported Windows path is **WSL2 with Ubuntu**.
Native Windows (PowerShell, no WSL) is marked **experimental /
not recommended**: the binary may build with the MSVC toolchain
but the operator surface (POSIX-style commands, firewall, service
management, mDNS / multi-interface routing) is not covered in any
SNIP docs and would only make sense for an operator willing to do
their own integration work.

WSL2 + client mode is the practical answer:

```powershell
# In an elevated PowerShell:
wsl --install -d Ubuntu
```

Then inside the WSL2 shell (Ubuntu):

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl git make

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"

git clone https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol.git
cd Storage-Node-Interface-Protocol
git checkout v0.4.0-rc4            # or <latest-release-candidate-tag>
                                   # Replace with `v0.4.0` once the final tag exists.
make release-check

./target/release/e2e-helper smoke --rpc-url https://rpc.sumchain.io --require-v2
```

Caveats for WSL2 client mode:

- Outbound RPC + libp2p dial work fine via WSL2's NAT.
- File paths between Windows and WSL2: keep SNIP's working
  directory inside the WSL2 filesystem (`/home/<user>/...`), not
  on `/mnt/c/...`. Cross-filesystem I/O is dramatically slower
  and has subtle case-sensitivity differences.
- Time sync inside WSL2 occasionally drifts. If `tx_status`
  polling starts behaving oddly, run `sudo hwclock -s` inside
  the WSL2 shell.

Archive mode under WSL2 is **not supported.** WSL2's networking
NATs inbound traffic; libp2p peers cannot reach a `listen`
process inside WSL2 from the public internet without explicit
Hyper-V port-forwarding configuration that SNIP does not document
and does not validate. Operators wanting to run an archive should
provision a Linux VPS.

### ChromeOS — recommended path is Crostini

Use the Linux development environment (Crostini) for client mode.
ChromeOS native (no Crostini) is not a SNIP target.

Enable Crostini via ChromeOS Settings → Developers → Linux
development environment. Then in the Linux shell:

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl git make

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"

git clone https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol.git
cd Storage-Node-Interface-Protocol
git checkout v0.4.0-rc4            # or <latest-release-candidate-tag>
                                   # Replace with `v0.4.0` once the final tag exists.
make release-check

./target/release/e2e-helper smoke --rpc-url https://rpc.sumchain.io --require-v2
```

Caveats for Crostini client mode:

- Outbound RPC and outbound libp2p dialing work; client `ingest`
  + `download` flows are exercised here without issue.
- Chromebook lid-close suspends the Crostini container along
  with the rest of the system. Long-running operations cannot
  survive a closed lid.
- File paths inside the container are isolated from ChromeOS;
  operate inside `/home/<user>/...`.

Archive mode under Crostini is **architecturally not supported.**
Inbound UDP from the WAN is gated by ChromeOS; there is no
documented way to expose a port from the Crostini container to
the public internet. Operators wanting to run an archive should
provision a Linux VPS.

## Promotion criteria (Experimental → Supported)

A cell promotes from ⚠️ Experimental to ✅ Supported when all of
the following land:

1. CI: `cargo build --workspace --release` and
   `cargo test --workspace --bins --lib` pass on the target on at
   least one supported runner image.
2. One operator (not the original developer) exercises the
   documented path end-to-end. For client cells: a full Public V2
   ingest + download round-trip, byte-identical. For archive
   cells: a continuous `listen` run reachable from mainnet peers
   for the validation window (default 7 days, longer if PoR
   cadence demands more samples).
3. Setup snippets in this document are confirmed accurate against
   the runner image / OS version validated.
4. Any platform-specific quirks observed during validation are
   documented in this file's "Per-environment setup" section.

## Not planned for v0.4.x

These items are deliberately deferred. Re-evaluate at the
`v0.5.x` planning cycle, or earlier if operator demand surfaces:

- **Native Windows archive support.** Service supervision
  (NSSM / `sc.exe`), Windows Defender Firewall rules, inbound
  UDP through corporate NICs, and the MSVC service-runtime
  posture are out of scope. Operators on Windows hosts should
  use a Linux VPS for archive duty.
- **Native Windows client tooling.** Building from source under
  raw PowerShell would require maintaining PowerShell-equivalent
  command snippets, native key-permission ACL recipes, and
  Windows path conventions throughout the docs. WSL2 sidesteps
  all of this for the cost of one `wsl --install`.
- **ChromeOS archive support.** Architecturally blocked by
  Chromebook inbound networking; no Google-documented API
  exposes Crostini ports to the WAN. Not planned without a
  ChromeOS-side change in policy.
- **macOS launchd plist templates.** Will land alongside the
  macOS archive promotion when its validation window closes.
- **Code signing, notarization, signed checksums.** `v0.4.x`
  ships a Linux x86_64 tarball and a SHA256SUMS file but does
  NOT sign binaries, notarize for macOS Gatekeeper, sign
  `SHA256SUMS` with PGP, or publish Sigstore attestations.
  Operators with threat models that require any of these should
  build from source at the matching tag. The full signing
  workstream is deferred to `v0.5.x`.
- **Prebuilt distribution beyond Linux x86_64.** `v0.4.x` ships
  a Linux x86_64 tarball only. Linux aarch64 and macOS arm64
  prebuilts are candidates for `v0.4.1+` once their per-cell
  promotion criteria clear. Homebrew, winget, scoop, deb/rpm
  packaging is a `v0.5.x+` workstream.
- **`cargo xtask release-check` cross-platform dev-loop wrapper.**
  `make release-check` via Git Bash / WSL2 / Crostini is enough.

Operator demand changes the calculus on any of these. File a
ticket with the specific use case if you have one.

## Permanent non-goals

These won't be revisited:

- **ChromeOS native (non-Crostini) SNIP.** No Linux container,
  no SNIP. Refer ChromeOS users to Crostini.
- **iOS / Android client.** Out of scope for the lifetime of
  `v0.4.x`. Separate product workstream if it ever happens.
