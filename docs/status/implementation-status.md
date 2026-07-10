# Implementation status

Current feature matrix. Every row lists whether the item is shipped
or planned, gives the SNIP-side entry point, and points at the
authoritative documentation. When the row's status depends on a
chain-side feature gate, the gate is named and the row records
where the current-day gate value can be verified.

If a row here disagrees with an archived plan document, this row
wins.

## Protocol

| Feature | Status | Location | Notes |
|---|---|---|---|
| V2 file lifecycle (`RegisterFilePendingV2` → push → `ActivateFileV2`) | Shipped | [`../protocol/v2-state-machine.md`](../protocol/v2-state-machine.md); [`crates/sum-node/src/ingest_v2.rs`](../../crates/sum-node/src/ingest_v2.rs) | Canonical mainnet write path. |
| V2 resume + abandon | Shipped | [`crates/sum-node/src/ingest_v2.rs`](../../crates/sum-node/src/ingest_v2.rs) `resume`, `abandon` | `abandon` covered by unit tests; no dedicated `tests/e2e_mirror.rs` scenario yet. |
| Public V2 upload + download | Shipped | [`../client/upload-and-download.md`](../client/upload-and-download.md) | |
| Private V2 upload + download (X25519 hybrid, chain-side per-recipient wrapped bundles) | Shipped | [`../security/privacy-audit.md`](../security/privacy-audit.md); [`crates/sum-node/src/download_private.rs`](../../crates/sum-node/src/download_private.rs); [`crates/sum-node/src/ingest_v2.rs`](../../crates/sum-node/src/ingest_v2.rs) | Supersedes the ChaCha20 + OOB-key proposal in [`../archive/SECURITY-ANALYSIS.md`](../archive/SECURITY-ANALYSIS.md). |
| V2 access control: `share`, `revoke`, `update-access` | Shipped | [`crates/sum-node/src/access.rs`](../../crates/sum-node/src/access.rs) | |
| Encryption key registration | Shipped | [`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs) `run_register_encryption_key` | |
| V2 assignment-aware challenge targeting | Shipped in `sum-chain`; gated by `assignment_targeting` | [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md); upstream `sum-chain` issue #97 | Gate value on any given deployment must be read at runtime via `chain_getChainParams`. SNIP does not restate the value as a constant. |
| Bounded coverage scheduling | **Planned / design-only** | Upstream `sum-chain` issue #81; `sum-chain:docs/specs/snip-assignment-aware-por-scheduling.md` | Separate mechanism from targeting. When it lands, archive-side responder behavior does not change. |
| V1 file lifecycle (legacy `ingest`) | Shipped | [`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs) `run_ingest` | Backwards-compatibility only. New files should register via V2. |
| PoR responder (`PorWorker`) | Shipped | [`crates/sum-node/src/por_worker.rs`](../../crates/sum-node/src/por_worker.rs); [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md) | Protocol-version-agnostic. |

## Networking

| Feature | Status | Location | Notes |
|---|---|---|---|
| mDNS (LAN discovery) | Shipped | [`crates/sum-net/src/discovery.rs`](../../crates/sum-net/src/discovery.rs) | Default. |
| Kademlia DHT + TCP fallback (WAN discovery) | Shipped | [`crates/sum-net/src/swarm.rs`](../../crates/sum-net/src/swarm.rs) | Gated by `--enable-wan`. |
| Circuit Relay v2 server + client, DCUtR | Shipped | [`crates/sum-net/src/nat.rs`](../../crates/sum-net/src/nat.rs) | Server mode gated by `--relay-server` (requires `--enable-wan`). |
| AutoNAT | Not implemented | — | Deferred with DCUtR-lite; may return if operator demand surfaces. |

## Storage + retention

| Feature | Status | Location | Notes |
|---|---|---|---|
| Deterministic chunk assignment (3× replication) | Shipped | [`crates/sum-store/src/assignment.rs`](../../crates/sum-store/src/assignment.rs) | R = 3 on mainnet per `chain_getChainParams.assignment_replication_factor`. |
| Garbage collection (mark-and-sweep with grace period) | Shipped | [`crates/sum-store/src/gc.rs`](../../crates/sum-store/src/gc.rs) | Pauses if L1 unreachable for > 5 min. |
| Health check | Shipped | [`crates/sum-store/src/lib.rs`](../../crates/sum-store/src/lib.rs) `health_check` | |
| Reed-Solomon erasure coding | **Planned** | [`../roadmap/roadmap.md`](../roadmap/roadmap.md) | Was Phase 4 Objective 3; no code today (no `ErasureCoder`, `ShardDescriptor`, or `reed-solomon-erasure` dependency). |

## Security + privacy

| Feature | Status | Location | Notes |
|---|---|---|---|
| Chain-side ACL enforcement | Shipped | [`../security/privacy-audit.md`](../security/privacy-audit.md) rows 8-10 | |
| Forward secrecy on revoke (key rotation) | **Planned** | [`../security/privacy-audit.md`](../security/privacy-audit.md) row 14; [`../roadmap/roadmap.md`](../roadmap/roadmap.md) | `revoke` removes the chain access entry but does not rotate `K_file`. |
| Fail-closed production profile | Shipped | [`crates/sum-node/src/profile.rs`](../../crates/sum-node/src/profile.rs) | |
| Log-privacy guardrail | Shipped | [`scripts/audit-logs.sh`](../../scripts/audit-logs.sh) | Runs on every `make release-check`. |

## Configuration + operator surface

| Feature | Status | Location | Notes |
|---|---|---|---|
| CLI `--chain-id` — live RPC derivation for `register-node` | Shipped | [`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs) `run_register_node` | `chain_id` read from `chain_getChainParams`; ignores `--chain-id` value. |
| CLI `--chain-id` — live RPC derivation for `IngestV2` and `Resume` (production profile) | Shipped | [`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs) `build_v2_ingest_params` | Production profile hard-fails when `chain_getChainParams` fails; dev profile falls back to `--chain-id`. |
| CLI `--chain-id` — live RPC derivation for `register-encryption-key`, `share`, `revoke`, `update-access`, `abandon`, and the listener-side `AssignmentAttestor` | **Planned** | See "Recommended follow-up" in the Phase 2 audit report | Today these code paths consume the CLI value directly. The workspace default is `1337`, which matches no documented environment (mainnet `chain_id = 1`, local-mirror `chain_id = 31337`). Documentation instructs operators to pass `--chain-id 1` for mainnet writes; wire this into runtime like `register-node` already does. |

## Packaging + release

| Feature | Status | Location | Notes |
|---|---|---|---|
| Linux x86_64 prebuilt tarball + install script | Shipped | [`../getting-started/install.md`](../getting-started/install.md) | v0.4.x. |
| Linux aarch64 prebuilt | **Planned** | [`../compatibility/platform-support.md`](../compatibility/platform-support.md) | v0.4.1+ pending operator validation. |
| macOS arm64 prebuilt | **Planned** | [`../compatibility/platform-support.md`](../compatibility/platform-support.md) | v0.4.1+. |
| Signed release artifacts (code signing, notarization, PGP-signed SHA256SUMS, Sigstore) | **Planned** | [`../compatibility/platform-support.md`](../compatibility/platform-support.md) "Not planned for v0.4.x" | v0.5.x workstream. |
| Homebrew / winget / scoop / deb / rpm | **Planned** | [`../compatibility/platform-support.md`](../compatibility/platform-support.md) | v0.5.x+ workstream. |

## Testing

| Feature | Status | Notes |
|---|---|---|
| Workspace unit + integration tests | Shipped | Runs under `cargo test --workspace`. |
| WS2b local-mirror E2E (`crates/sum-node/tests/e2e_mirror.rs`) | Shipped | Requires chain compose preset. |
| `abandon` mirror-E2E scenario | **Planned** | `abandon` is unit-tested but has no dedicated `tests/e2e_mirror.rs` scenario. |
| WAN discovery integration test | **Planned** | Requires multi-network setup. |
| Live-validator PoR loop integration test | **Planned** | Requires validators + funded operators. |

## How to update this file

Add a row when you ship a feature. Add a row when you file a
planned-item issue. Do not remove rows silently — status changes
from "Planned" to "Shipped" should stay recorded, and CHANGELOG
carries the historical timing.
