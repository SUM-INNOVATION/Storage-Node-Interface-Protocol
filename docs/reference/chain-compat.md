# Chain compatibility

SNIP is a client of SUM Chain. Wire-format compatibility — bincode-v1
transaction payloads and JSON-RPC response shapes — is pinned by
tests in this repo and MUST stay byte-for-byte aligned with whatever
chain release SNIP is built against. This document is the
operator-facing source of truth for how SNIP and chain stay aligned.

It keeps two things apart:

- **Production-network compatibility** (below): the chain wire
  format SNIP is confirmed against, and the mainnet facts that were
  read from the public network, with when and how.
- **Local-mirror requirements** ([below](#local-mirror-requirements)):
  the synthetic devnet SNIP's end-to-end suite runs against. It is
  not mainnet, and a passing mirror run is not evidence of anything
  about mainnet.

Dates in this document are UTC.

## Production-network compatibility

### Pinned chain version

| Field                                 | Value                                                |
|---------------------------------------|------------------------------------------------------|
| Chain commit (compatibility pin)      | `5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0`           |
| What it identifies                    | a chain **source** state: the wire format SNIP's fixtures were confirmed against. Not the validators' binary or their runtime configuration |
| Live-chain `v2_enabled_from_height`   | read from the live chain; see "Mainnet pin / deployed chain" |

The chain repository (`SUM-INNOVATION/sum-chain`) is public
(checked 2026-10-18). The pin is maintained by release managers.
What it does and does not identify:

- **Which binary the mainnet validators run:** not identified. This
  document does not track that.
- **Mainnet's genesis block: reproduced by the pin's root
  `genesis.json`.** That file (`chain_id` 1, `genesis_time` 0, two
  validators) produces mainnet's genesis block hash exactly; see
  "Network identity" below. It is not the validators' runtime
  genesis file: it has no V2 gate, while mainnet reports
  `v2_enabled_from_height` 5200000, because activation heights are
  added to the runtime file, and the genesis block does not depend
  on them.
- **`genesis/mainnet_genesis.json` is not mainnet's genesis.** At the
  pin it is a template: its five validators and five allocations are
  placeholders, and the chain later labelled the file "TEMPLATE
  ONLY — production validators boot from the root runtime
  genesis.json, NOT this file" (chain commit `2e53e46d`). It does not
  produce a genesis block at all.

This SHA is the load-bearing identifier operators and reviewers
use to say "SNIP's wire format is confirmed against THIS chain
source state."

Chain team confirmed the V2 wire-format fixture suite passes
against this commit:

```
cargo test -p sumchain-primitives --test v2_wire_fixtures
# 18 passed / 0 failed
```

The 18-fixture surface mirrors the `tx_builder::tests::fixture_*`
and `rpc_client::contract_tests::*` constants pinned in this repo
(see "Transaction payload fixtures" and "RPC contract tests"
below). Wire-shape divergence between the two surfaces is what
this pin is meant to detect; both sides green means the V2 wire
shape is confirmed-stable on this chain commit.

Bumping requires a chain-team-coordinated re-pin (see "Re-pinning"
below). The "no signing material in any committed artifact" rule
applies to every future re-pin: the SHA must be from a chain
history that contains no validator keys, dev seeds, faucet
privates, or any other signing material.

Re-pinning procedure is in [`RELEASE-CHECKLIST.md`](../release/release-checklist.md).

### Mainnet pin / deployed chain

Public facts about the deployed mainnet. These values are
wire-protocol observable and safe to publish; chain-team-private
fields (validator hostnames, RPC admin endpoints, validator binary
SHA) deliberately do not appear in this table. Rows marked
*verified 2026-10-18* were read from the public RPC with the
read-only smoke (`make smoke RPC=https://rpc.sumchain.io
SMOKE_ARGS=--require-v2`, run twice, at 04:55:47Z and 04:56:48Z).
They record what that endpoint reported then. They are not a claim
that mainnet runs the compatibility pin.

| Field                                  | Value                                                       |
|----------------------------------------|-------------------------------------------------------------|
| `chain_id`                             | `1` (*verified 2026-10-18*)                                 |
| Public RPC                             | `https://rpc.sumchain.io`                                   |
| SNIP compatibility pin                 | `5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0` (wire format; see above) |
| Genesis block hash                     | `0x1156d350e7d0ac45cb96bfca25d57c71675ceea5949ea44d81432d08baed68e6` (*verified 2026-10-18*, block 0 from the public RPC, and reproduced offline from the pin's root `genesis.json`). See "Network identity" below |
| `v2_enabled_from_height`               | `5200000` (*verified 2026-10-18*)                           |
| `finality_depth`                       | `6` blocks (*verified 2026-10-18*)                          |
| `block_time_ms`                        | `3000`, as `chain_getChainParams` reports it (*verified 2026-10-18*). The block producer's pacing parameter, not a consensus rule: the chain excludes it from its consensus-config commitment ("producer pacing only"), and no SNIP code path consumes it. |
| Observed block cadence                 | ≈ 1.5 s: the finalized height advanced 40 blocks in 61 s (*observed 2026-10-18*). Use this, not `block_time_ms`, for wall-clock estimates. |
| V2 live since                          | finalized height ≥ `5200000`                                |

The compatibility pin is authoritative for wire format. Release builds are not byte-
reproducible across hosts (toolchain version, build flags, system
libraries vary), so the validator binary SHA is intentionally
NOT listed here as a reproducibility requirement — operators
should treat the chain commit as the canonical reference and
trust the chain team's release tooling for binary distribution.

#### Network identity

SNIP reads no genesis file, and no SNIP check compares a genesis
hash. The only network identity SNIP uses is `chain_id`:
transactions are signed for it, and operators confirm it with
`chain_getChainParams` (`make smoke`) before any write. None of that
changes here.

**Withdrawn value.** Versions of this document from 2026-05-07 to
2026-10-18 listed a "Genesis SHA-256", `040b1fd3…ec431`, described
as the hash of the genesis file at the pin. Its source was not
recorded, and it could not be reproduced (checked 2026-10-18):

- it is not the SHA-256 of any file at a genesis path anywhere in
  the chain repository's history (every such blob on every
  branch);
- the pin's `genesis/mainnet_genesis.json` hashes to `cad1255f…`,
  and that file is the placeholder template described above;
- it is not the validator genesis-file hash in the chain's
  production runbook
  ([`stage1-native-runbook.md`](https://github.com/SUM-INNOVATION/sum-chain/blob/d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2/docs/operations/stage1-native-runbook.md)
  § 1, one validator, inspected 2026-09-26), which that runbook
  itself records as differing from the committed file.

It is withdrawn, not replaced.

**A genesis-file hash does not identify the network.** Activation
heights are edited into each validator's runtime `genesis.json`
(the chain's `docs/index.md`: the 9,200,000 cohort was "added to
runtime genesis after the 8,900,000 batch"), so the file's SHA-256
changes at each such upgrade while the network stays the same. The
chain builds its genesis block from the genesis time, the
allocations' state root and the genesis proposer
(`create_genesis_block` in the chain's `crates/genesis/src/lib.rs`),
not from those parameters. `chain_id` plus the genesis block hash
is the stable identity.

**Mainnet's network identity: `chain_id` 1 and genesis block hash
`0x1156d350e7d0ac45cb96bfca25d57c71675ceea5949ea44d81432d08baed68e6`.**

- **Source:** block 0 from the public RPC (`sum_getBlockByHeight`
  with height `0`, read-only, at 2026-10-18T23:11:44Z). It reported
  that hash, timestamp `0`, state root
  `0x5fa18c9e8b229ac4cec3aca0b28eba87e2bb1b54c249510f16f084c421511ff2`
  and no transactions. The hash is BLAKE3 over the bincode encoding
  of the header (header v1).
- **Offline reproduction (2026-10-18):** the chain's own
  `Genesis::create_genesis_block`, run over every `chain_id` 1
  genesis file in the chain repository's history (all refs) and the
  `genesis/` files at the pin and at `d8130ead`.
  - **Reproduces it:** the root `genesis.json`, the only version of
    that file (blob `2726051d`). It was added on 2026-01-02 and is
    unchanged at the pin `5ff6c748` and at chain `main` `d8130ead`.
    The hash, state root, proposer, timestamp and transaction count
    all match, under both the pin's code and `d8130ead`'s.
  - **Does not:** `genesis/mainnet_genesis.json` at either commit
    (placeholder validators and allocations, so no block can be
    built). Every other genesis file in the history is for chain
    1337 or 31337.
- **What this establishes:** mainnet's genesis block is the one the
  root `genesis.json` at the pin defines: the same genesis time,
  allocations (state root) and genesis proposer. The genesis block
  does not cover `chain_id`; the file's `chain_id` 1 agrees with what
  the RPC reports. It does not establish the
  validators' binary, their runtime genesis file or its activation
  heights, or the current validator set, and it does not make the
  pin a statement about any of them. The compatibility pin remains a
  wire-format pin; this changes no check and no pin.

### Transaction payloads SNIP submits to mainnet

Both payload kinds below are pinned by the bincode-v1 fixtures
listed under "Transaction payload fixtures" further down. The
TxPayload tag indices are stable on this chain commit; a diff is
a hard chain-compat break.

**Archive-node registration** (V1 path, used today):

```text
TxPayload::NodeRegistry(NodeRegistryOperation::Register {
    role: ArchiveNode,
    stake,
})
TxPayload tag: 17
```

Archive registration deliberately stays on V1 `NodeRegistry`.
`NodeRegistryV2` exists at TxPayload tag `19` but is currently
scoped to encryption-key registration only — there is no V2
archive-registration op today. If the chain ever adds one, SNIP
will pin a new fixture and update this section in the same
release.

**Encryption key registration** (V2 path):

```text
TxPayload::NodeRegistryV2(NodeRegistryOperationV2::RegisterEncryptionKey {
    encryption_pubkey,
})
TxPayload tag: 19
```

Required for any address that wants to receive Private V2 file
shares. The X25519 pubkey is HKDF-derived from the operator's
Ed25519 seed (domain `snip-x25519-encryption-key-v1`); the seed
itself never reaches the chain.

### RPC methods SNIP intentionally uses

These are the JSON-RPC methods SNIP calls against any compatible
chain (mainnet, local-mirror, or future replica). Aliases that
exist on the chain but SNIP does NOT use are listed below for
clarity — operators reading chain logs may see other methods
flowing, but SNIP itself stays on this set.

| Method                              | Used for                                                       |
|-------------------------------------|----------------------------------------------------------------|
| `send_raw_transaction`              | submit signed bincode-v1 tx (returns tx hash)                  |
| `chain_getTransactionStatus`        | poll for `Finalized` / `Failed` / `Dropped` after submission   |
| `storage_getFileInfoV2`             | resolve V2 chain row (visibility, lifecycle, access list)      |
| `storage_getActiveNodesAtHeight`    | snapshot of node records at a chain height; SNIP filters by the eligibility contract on the SNIP side (see below) |
| `chain_getChainParams`              | read `chain_id`, `assignment_replication_factor`, etc.         |
| `account_getEncryptionPublicKey`    | resolve a recipient's registered X25519 pubkey                 |
| `chain_getBlockHeight`              | read finalized head for V2 enablement gate + finality budgets  |
| `get_balance`, `get_nonce`          | preflight + tx assembly                                        |

The mainnet chain also exposes alias methods (`sum_sendRawTransaction`
and per-tx receipt aliases). **SNIP does not use them.** Staying on
the canonical `send_raw_transaction` + `chain_getTransactionStatus`
pair keeps the wire surface SNIP depends on small and review-able;
an alias divergence on the chain side is then a no-op for SNIP.

### NodeRecord role and status wire shape

The chain formats `NodeRecordInfo.role` and `.status` via
`format!("{:?}", …)` over its internal enums (upstream
`sum-chain/crates/rpc/src/server.rs:7595-7839`). The current strings
SNIP recognises are:

- `role`: `"ArchiveNode"`, `"Validator"`.
- `status`: `"Active"`, `"Slashed"`, `"Unbonding"`, `"Withdrawn"`.

Both fields deserialize as `String`, so any future chain-added
variant lands as its raw string without a SNIP-side release — the
value stays observable through `NodeRecordInfo.role` / `.status`
and is treated as ineligible for assignment. The eligibility
contract (`role == "ArchiveNode" && status == "Active"`) is
described in
[`../architecture/chain-integration.md`](../architecture/chain-integration.md)
"Active-archive eligibility contract."

## Local-mirror requirements

The local mirror is the chain's **synthetic five-validator
devnet**: `deploy/snip-local-mirror.yaml` in the chain repository,
provisioned by the chain's own `setup-local-testnet --mode topology
--validator-count 5`. It exists to test SNIP end to end against real
chain code. It is NOT mainnet, it says nothing about which network
or binary mainnet runs, and a passing run is not evidence of a
mainnet match.

| Requirement                    | Value                                                   |
|--------------------------------|---------------------------------------------------------|
| Chain revision                 | `SUMCHAIN_COMMIT` in [`.github/workflows/release-local-mirror-e2e.yml`](../../.github/workflows/release-local-mirror-e2e.yml), currently `d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2` |
| Immutable                      | a full commit hash; [`scripts/local-mirror-e2e.sh`](../../scripts/local-mirror-e2e.sh) refuses a chain checkout at any other commit |
| Tested                         | a chain `main` commit on which every reported check run succeeded (21 of 21, including `build-test-clippy` on x86_64 and aarch64 and `supply-chain-audit`; checked 2026-10-18). On GitHub-hosted runners its devnet builds, all five validators become ready, and the read-only smoke passes (2026-10-18) |
| Relation to the pin            | independent. Changing `SUMCHAIN_COMMIT` is not a re-pin, and a re-pin does not change `SUMCHAIN_COMMIT`; each is its own reviewed change |
| RPC                            | `http://localhost:8545` (validator-1)                   |
| `chain_id`                     | `1337`, from the generated genesis (verify at runtime via `chain_getChainParams`) |
| `v2_enabled_from_height`       | `0` (V2 from genesis)                                   |
| Block cadence                  | ~2 s (`block_time_ms` 2000 in the provisioner)          |
| Keys                           | throwaway, generated on the host that runs it into the ignored `devnet/`; never committed, printed or uploaded |
| Where it runs                  | the `release local-mirror E2E` workflow on a fresh GitHub-hosted runner, for release pull requests; or a Docker-capable Linux host the owner names ([`RELEASE-CHECKLIST.md`](../release/release-checklist.md) § 5) |

**Why not the compatibility pin.** At `5ff6c748` the chain's
preset mounts `genesis/snip-mirror-genesis.json` and
`keys/validator1.json`, neither of which is tracked at that commit,
so the mirror cannot be brought up from the repository at the pin
(checked 2026-10-18).

### Mainnet vs local-mirror

Operator-visible differences:

| Field                          | Mainnet                  | Local mirror             |
|--------------------------------|--------------------------|--------------------------|
| `chain_id`                     | `1`                      | `1337`                  |
| `v2_enabled_from_height`       | `5200000`                | `0` (V2 from genesis)    |
| Block cadence                  | ≈ 1.5 s observed (`block_time_ms` reports 3000) | ~2 s (`block_time_ms` 2000) |
| Pre-funded test addresses      | none                     | yes (the provisioner's funded roles) |

Operators MUST verify `chain_id` at runtime via
`chain_getChainParams` (or `make smoke`) before any tx submission.
Signing the wrong `chain_id` means the chain rejects the tx and
the fee is burned. The fastest way to catch a mis-configured RPC
URL is to gate on the smoke check before any first write.

### Local-mirror posture

The local-mirror chain emits `v2_enabled_from_height: 0`, i.e. "V2
enabled from genesis." Tests SHOULD assert that SNIP deserializes
this as `Some(0)` and admits — not as `None` and not as a bare `u64`
that silently defaults.

The mirror's documented `chain_id` is `1337`, recorded in the
table above. Operators SHOULD still call
`chain_getChainParams` against the running mirror at the start of
any tx-signing test and abort if the returned id doesn't match —
catches the case where a future mirror release silently bumps the
id (e.g., dev → stage), which would otherwise let SNIP sign with
a stale chain id.

**Self-bootstrapping is a hard requirement.** The mirror's
validator keys are generated on the host that runs it
(`setup-local-testnet --mode topology`, into the ignored `devnet/`);
they are never committed to the chain repo, and SNIP must never
consume an artifact that ships keys. The same rule applies
prospectively to every future change of `SUMCHAIN_COMMIT` and
every future re-pin.

The canonical truth for "SNIP-side correctness against the chain
wire shape" remains the in-tree fixture + contract test surface
listed below; the local-mirror E2E suite (WS2) is the operator
gate that confirms SNIP and the chain agree end-to-end at
`SUMCHAIN_COMMIT`. The in-tree tests are stable, exhaustive for V2
operations, and run on every PR via the linux CI gate; the WS2
mirror suite is run before each release.

## V2 enablement gate (load-bearing)

`chain_getChainParams.v2_enabled_from_height` is `Option<u64>` on
the SNIP side. Three semantically distinct states:

| Chain JSON  | SNIP value | Meaning                                                |
|-------------|------------|--------------------------------------------------------|
| `0`         | `Some(0)`  | V2 enabled from genesis. Any finalized height passes.  |
| `N` (N ≥ 1) | `Some(N)`  | V2 enabled at finalized height ≥ N. Below N: refuse.   |
| `null`      | `None`     | V2 disabled on this chain. Refuse all V2 submissions.  |
| field absent| `None`     | Older chain that doesn't know about V2. Refuse.        |

The canonical client predicate:

```rust
match params.v2_enabled_from_height {
    Some(h) => finalized_height >= h,   // admit
    None    => false,                    // refuse
}
```

`Some(0)` is **NOT** the same as `None`. A future "let's just default
null to 0 to simplify" regression would silently flip a V2-disabled
chain into a V2-permitting one and burn fees on submissions the
chain will reject. The `chain_params_v2_enabled_from_height_zero_is_some_zero`
test in [`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs)
exists specifically to catch that regression.

## Pinned wire-format constants (public protocol)

These are protocol-level facts of the V2 chain ABI. They do not
identify any environment.

### TxPayload tags

| Variant            | Tag |
|--------------------|----:|
| `NodeRegistryV2`   | 19  |
| `StorageMetadataV2`| 20  |

### V2 op order (within `StorageMetadataV2`)

| Op                       | Index |
|--------------------------|------:|
| `RegisterFilePendingV2`  | 0     |
| `ActivateFileV2`         | 1     |
| `AbandonFileV2`          | 2     |
| `AcceptAssignmentV2`     | 3     |
| `AddAccessV2`            | 4     |
| `RemoveAccessV2`         | 5     |
| `UpdateAccessV2`         | 6     |

Variant-tag stability is enforced by the `v2_op_variant_indices_are_stable`
and `payload_v2_variant_indices_are_stable` tests in
[`crates/sum-node/src/tx_builder.rs`](../../crates/sum-node/src/tx_builder.rs).
A diff to these tests is a hard chain-compat break and MUST land in
its own commit referencing the chain release tag.

## Transaction payload fixtures

The bincode-v1 byte layout of every V2 transaction SNIP submits is
pinned by deterministic fixtures in
[`crates/sum-node/src/tx_builder.rs`](../../crates/sum-node/src/tx_builder.rs):

- `fixture_register_encryption_key_bytes`
- `fixture_register_file_pending_v2_bytes`
- `fixture_activate_file_v2_bytes`
- `fixture_abandon_file_v2_bytes`
- `fixture_accept_assignment_v2_bytes`
- `fixture_add_access_v2_bytes`
- `fixture_remove_access_v2_bytes`
- `fixture_update_access_v2_bytes`
- `fixture_update_access_v2_clear_expires_at_bytes`
- `fixture_tx_payload_node_registry_v2_register_encryption_key`
- `fixture_tx_payload_storage_metadata_v2_accept_assignment`
- `fixture_tx_payload_storage_metadata_v2_activate_and_abandon`
- `fixture_tx_payload_storage_metadata_v2_add_remove_update`

These are protocol-test constants, not environment details: they
encode what bytes go on the wire for a given transaction shape. Any
diff to a fixture is a wire-format change that requires
chain-team coordination.

## RPC contract tests

Pinned in [`crates/sum-node/src/rpc_client.rs`](../../crates/sum-node/src/rpc_client.rs)
under `rpc_client::contract_tests`:

- `storage_get_file_info_v2_decodes_success_shape`
- `storage_get_file_info_v2_jsonrpc_error_surfaces_as_err`
- `storage_get_file_info_v2_null_result_is_decode_error`
- `account_get_encryption_public_key_chain_canonical_shape`
- `account_get_encryption_public_key_null_is_none`
- `account_get_encryption_public_key_invalid_payload_errors`
- `account_get_encryption_public_key_tolerates_legacy_shapes`

`ChainParamsInfo` deserialization is pinned in
[`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs):

- `chain_params_decodes_canonical_shape`
- `chain_params_v2_enabled_from_height_null_is_none`
- `chain_params_v2_enabled_from_height_missing_is_none`
- `chain_params_v2_enabled_from_height_zero_is_some_zero`

## Re-pinning when chain delivers a new tip SHA

1. Chain ops delivers a new internal tip SHA out-of-band. The SHA
   MUST be from a history that contains no committed signing
   material; if chain rewrote history to scrub leaked secrets,
   confirm the rewrite landed before pinning.
2. Regenerate the bincode-v1 fixtures against the new chain types
   (chain-team responsibility — they own the type definitions
   SNIP mirrors). Chain team should report the result of their
   own V2 wire-fixture suite (e.g.
   `cargo test -p sumchain-primitives --test v2_wire_fixtures`)
   on the new SHA before SNIP pins.
3. Run `cargo test -p sum-node tx_builder rpc_client` and
   `cargo test -p sum-types`. Any diff in fixture bytes or
   deserialization tests is an intentional wire-format change and
   MUST land in a separate commit referencing the new chain SHA.
4. Update the "Pinned chain version" row at the top of this doc
   with the actual SHA. Do NOT pin against any history that
   retains committed signing material; do NOT echo a superseded
   SHA in commit messages or doc bodies — SNIP's public history
   should not retain a pointer to compromised or scrubbed state.
5. Run `make release-check` (linux CI gate). A re-pin does not
   change the local mirror's `SUMCHAIN_COMMIT` (see
   "Local-mirror requirements"); moving the mirror to another chain
   revision is a separate reviewed change.
6. Update [`CHANGELOG.md`](../../CHANGELOG.md).
