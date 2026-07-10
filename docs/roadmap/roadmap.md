# SNIP roadmap

Forward-looking items. Everything here is planned or design-only.
Once an item ships, it moves out of this file and into
[`../status/implementation-status.md`](../status/implementation-status.md)
as a "Shipped" row.

If an item here has an upstream issue, the issue is the authority.
When it has no issue yet, this file names the acceptance criteria
so a future contributor knows what "done" looks like.

## Near-term (candidates for v0.4.1 → v0.4.x)

### Runtime: read `chain_id` from RPC for every V2 tx-signing path

**Motivation**: today several tx-signing paths — `register-encryption-key`,
`share`, `revoke`, `update-access`, `abandon`, the ingest-v2 dev-
fallback branch, and the listener-side `AssignmentAttestor` — read
`chain_id` from the CLI `--chain-id` value. The workspace default
is `1337`; mainnet is `1`; local-mirror is `31337`. Documentation
instructs mainnet operators to pass `--chain-id 1` on every write
example, but nothing prevents an omission.

**Reference implementation**: `run_register_node` at
[`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs)
already reads `chain_id` live from RPC via `rpc.get_chain_id()`.

**Acceptance criteria**:
- `AssignmentAttestor::new` no longer accepts a `chain_id` parameter;
  it reads `chain_id` from `chain_getChainParams` at construction.
- `run_register_encryption_key`, `run_share`, `run_revoke`,
  `run_update_access`, `run_abandon_v2`, and the dev-fallback
  branch of `build_v2_ingest_params` all read `chain_id` from RPC.
- The comment at `crates/sum-node/src/main.rs:683-686` naming this
  as a W10 placeholder is removed.
- Chain-facing wire fixtures under
  [`crates/sum-node/src/tx_builder.rs`](../../crates/sum-node/src/tx_builder.rs)
  continue to pin the same bytes (any diff is a chain-compat break
  and would land in a separate commit).
- Documentation drops the "add `--chain-id 1` explicitly" callouts;
  the CLI flag is retained but becomes an override, not a required
  input on mainnet.

**Cross-references**: this item is filed only in this roadmap,
per the Phase 2 audit's non-mutation constraint on external issues.
When a maintainer creates the tracking issue, link it here.

### Linux aarch64 prebuilt tarball

Cross-compile lane in the release workflow. Requires one operator's
long-run validation before promoting; see
[`../compatibility/platform-support.md`](../compatibility/platform-support.md)
"Promotion criteria."

### macOS arm64 prebuilt tarball

Requires one operator's long-run archive validation on Apple
Silicon. macOS Gatekeeper handling (developer signing / notarization)
is a separate item below.

### `abandon` local-mirror E2E scenario

`abandon` has strong unit coverage under
[`crates/sum-node/src/ingest_v2.rs`](../../crates/sum-node/src/ingest_v2.rs)
(`abandon_wrong_lifecycle_returns_not_admissible`,
`abandon_at_grace_boundary`, `abandon_just_past_grace_finalizes`,
etc.) but no dedicated scenario in
[`crates/sum-node/tests/e2e_mirror.rs`](../../crates/sum-node/tests/e2e_mirror.rs).
Add one that lands the file in `Pending`, waits past
`activation_grace_blocks`, and confirms the abandon lifecycle
against the running mirror.

## Medium-term (candidates for v0.4.2+ / v0.5.x)

### Forward secrecy on revoke — `K_file` rotation

`revoke` today removes the chain access entry but does not rotate
`K_file`. A revoked recipient who cached the ciphertext and their
old bundle can still decrypt past content. Row 14 of
[`../security/privacy-audit.md`](../security/privacy-audit.md)
documents this. Rotation would either re-ingest the file under a
fresh `K_file` (owner-driven), or add a chain-side rotation
primitive that superseded the wrapped bundle. Both are protocol
design decisions.

### Bounded coverage scheduling on the chain

Tracked upstream as `sum-chain` issue #81; specification at
`sum-chain:docs/specs/snip-assignment-aware-por-scheduling.md`.
When it lands, SNIP's responder does not change; the observable
challenge distribution over time does. See
[`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md)
for the distinction between targeting and scheduling.

### Reed-Solomon erasure coding

Replace 3× chunk replication with `k=4` data + `m=2` parity coding.
Storage overhead drops from ~3× to ~1.5×. Requires chain-side
challenge granularity change from chunks to shards; the plan lives
in [`../archive/PHASE4-EXECUTION-PLAN.md`](../archive/PHASE4-EXECUTION-PLAN.md)
"Objective 3." Not started; no `ErasureCoder`, `ShardDescriptor`,
or `reed-solomon-erasure` dependency in the tree today.

### Lightweight-swarm client mode

`--client` today exits after upload and skips the serve loop, but
still spins up a full libp2p swarm with gossipsub subscriptions
and a listening socket. A true outbound-only mode would skip the
listen socket and gossipsub subscriptions entirely — see
[`../archive/CLIENT-MODE-GAP.md`](../archive/CLIENT-MODE-GAP.md)
"Phase 3."

### AutoNAT / DCUtR-lite

Deferred with the initial WAN work. May return if operator demand
around NAT-heavy peer topologies surfaces.

## Longer-term (v0.5.x+)

### Signed release artifacts

Code signing for macOS (Developer ID + notarization), signed
`SHA256SUMS` (minisign or PGP), Sigstore attestations. Deferred
per [`../compatibility/platform-support.md`](../compatibility/platform-support.md)
"Not planned for v0.4.x."

### Packaging beyond raw tarballs

Homebrew, winget, scoop, deb / rpm. Requires the signing
workstream to land first.

### Native Windows archive support

Explicitly deferred; see the same "Not planned for v0.4.x" section
of the compatibility doc. WSL2 remains the documented Windows
client path.

## How to add to this file

- If the item has an upstream issue, link it in the first
  paragraph.
- If it has none, name the acceptance criteria in a
  sub-list so a future implementer knows what "done" looks like.
- Once shipped: move the row to
  [`../status/implementation-status.md`](../status/implementation-status.md)
  as a "Shipped" line and remove it from here. CHANGELOG carries
  the timing.
