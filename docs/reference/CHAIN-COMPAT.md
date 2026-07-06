# Chain compatibility

SNIP is a client of SUM Chain. Wire-format compatibility — bincode-v1
transaction payloads and JSON-RPC response shapes — is pinned by
tests in this repo and MUST stay byte-for-byte aligned with whatever
chain release SNIP is built against. This document is the
operator-facing source of truth for how SNIP and chain stay aligned.

> **The chain repository is private.** The exact chain commit is
> maintained out-of-band by release managers. Public SNIP releases
> pin an internal chain release tag, not a raw private SHA. Operators
> and reviewers should treat the chain commit as a managed dependency
> reference, similar to how a binary release is pinned by tag rather
> than by source URL.

## Pinned chain version

| Field                                 | Value                                                |
|---------------------------------------|------------------------------------------------------|
| Chain commit (internal private chain) | `5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0`           |
| Local-mirror RPC                      | `http://localhost:8545`                              |
| Local-mirror `chain_id`               | `1337` (verify at runtime via `chain_getChainParams`) |
| Local-mirror `v2_enabled_from_height` | emits `0` (V2 enabled from genesis)                  |
| Local-mirror block cadence            | ~2 seconds                                           |
| Live-chain `v2_enabled_from_height`   | published with the live release tag                  |

The chain commit recorded here is an internal private-chain
reference. The chain repository, branch, and release tooling are
not public; this SHA is the load-bearing identifier operators and
reviewers use to say "SNIP is built against THIS chain state."

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

Re-pinning procedure is in [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md).

## Mainnet pin / deployed chain

Confirmed-public facts about the deployed mainnet at the pinned
chain commit. These values are wire-protocol observable and safe
to publish; chain-team-private fields (validator hostnames, RPC
admin endpoints, validator binary SHA) deliberately do not appear
in this table.

| Field                                  | Value                                                       |
|----------------------------------------|-------------------------------------------------------------|
| `chain_id`                             | `1`                                                         |
| Public RPC                             | `https://rpc.sumchain.io`                                   |
| Chain commit                           | `5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0`                  |
| Genesis SHA-256                        | `040b1fd32bfab5008c8c6c048e853fa6717db82b5a3af56cab494f88fc1ec431` |
| `v2_enabled_from_height`               | `5200000`                                                   |
| `finality_depth`                       | `6` blocks                                                  |
| `block_time_ms`                        | `3000` (≈ 3 s)                                              |
| V2 live since                          | finalized height ≥ `5200000`                                |

The chain commit is authoritative. Release builds are not byte-
reproducible across hosts (toolchain version, build flags, system
libraries vary), so the validator binary SHA is intentionally
NOT listed here as a reproducibility requirement — operators
should treat the chain commit as the canonical reference and
trust the chain team's release tooling for binary distribution.

Genesis SHA-256 is the SHA-256 of the canonical genesis JSON file
the chain ships at the pinned commit. Operators bringing up a
node SHOULD verify their local genesis matches this hash before
syncing — a mismatched genesis means a different chain.

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
| `storage_getActiveNodesAtHeight`    | snapshot of `ArchiveNode/Active` rows at a chain height        |
| `chain_getChainParams`              | read `chain_id`, `assignment_replication_factor`, etc.         |
| `account_getEncryptionPublicKey`    | resolve a recipient's registered X25519 pubkey                 |
| `chain_getBlockHeight`              | read finalized head for V2 enablement gate + finality budgets  |
| `get_balance`, `get_nonce`          | preflight + tx assembly                                        |

The mainnet chain also exposes alias methods (`sum_sendRawTransaction`
and per-tx receipt aliases). **SNIP does not use them.** Staying on
the canonical `send_raw_transaction` + `chain_getTransactionStatus`
pair keeps the wire surface SNIP depends on small and review-able;
an alias divergence on the chain side is then a no-op for SNIP.

### Mainnet vs local-mirror

The local-mirror chain (compose preset at the pinned commit) is a
single-validator devnet, NOT mainnet. Operator-visible differences:

| Field                          | Mainnet                  | Local mirror             |
|--------------------------------|--------------------------|--------------------------|
| `chain_id`                     | `1`                      | `1337`                   |
| `v2_enabled_from_height`       | `5200000`                | `0` (V2 from genesis)    |
| Block cadence                  | ~3 s                     | ~2 s                     |
| Pre-funded test addresses      | none                     | yes (overlay-funded)     |

Operators MUST verify `chain_id` at runtime via
`chain_getChainParams` (or `make smoke`) before any tx submission.
Signing the wrong `chain_id` means the chain rejects the tx and
the fee is burned. The fastest way to catch a mis-configured RPC
URL is to gate on the smoke check before any first write.

> **Local mirror is runnable at this pin.** The compose preset
> at `deploy/snip-local-mirror.yaml` in the chain repo, checked
> out at the pinned SHA, brings up a single-validator devnet
> with V2 enabled from genesis. Validator key is generated at
> first boot into a Docker named volume — no signing material is
> committed. The SNIP-side WS2 suite that drives this mirror
> end-to-end is the next workstream; until it lands, operators
> can still smoke-check the running mirror via
> `make smoke RPC=http://localhost:8545`. See
> [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md) for bring-up /
> stop / wipe commands and
> [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) § 5 for the
> release-flow integration.

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
test in [`crates/sum-types/src/rpc_types.rs`](../crates/sum-types/src/rpc_types.rs)
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
[`crates/sum-node/src/tx_builder.rs`](../crates/sum-node/src/tx_builder.rs).
A diff to these tests is a hard chain-compat break and MUST land in
its own commit referencing the chain release tag.

## Transaction payload fixtures

The bincode-v1 byte layout of every V2 transaction SNIP submits is
pinned by deterministic fixtures in
[`crates/sum-node/src/tx_builder.rs`](../crates/sum-node/src/tx_builder.rs):

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

Pinned in [`crates/sum-node/src/rpc_client.rs`](../crates/sum-node/src/rpc_client.rs)
under `rpc_client::contract_tests`:

- `storage_get_file_info_v2_decodes_success_shape`
- `storage_get_file_info_v2_jsonrpc_error_surfaces_as_err`
- `storage_get_file_info_v2_null_result_is_decode_error`
- `account_get_encryption_public_key_chain_canonical_shape`
- `account_get_encryption_public_key_null_is_none`
- `account_get_encryption_public_key_invalid_payload_errors`
- `account_get_encryption_public_key_tolerates_legacy_shapes`

`ChainParamsInfo` deserialization is pinned in
[`crates/sum-types/src/rpc_types.rs`](../crates/sum-types/src/rpc_types.rs):

- `chain_params_decodes_canonical_shape`
- `chain_params_v2_enabled_from_height_null_is_none`
- `chain_params_v2_enabled_from_height_missing_is_none`
- `chain_params_v2_enabled_from_height_zero_is_some_zero`

## Local-mirror posture

The local-mirror chain emits `v2_enabled_from_height: 0`, i.e. "V2
enabled from genesis." Tests SHOULD assert that SNIP deserializes
this as `Some(0)` and admits — not as `None` and not as a bare `u64`
that silently defaults.

The mirror's documented `chain_id` is `1337`, recorded in the
"Pinned chain version" table above. Operators SHOULD still call
`chain_getChainParams` against the running mirror at the start of
any tx-signing test and abort if the returned id doesn't match —
catches the case where a future mirror release silently bumps the
id (e.g., dev → stage), which would otherwise let SNIP sign with
a stale chain id.

**Self-bootstrapping is a hard requirement.** The mirror at this
pin generates validator keys at runtime into a Docker named
volume; the keys are never committed to the chain repo, and SNIP
must never consume an artifact that ships keys. The same rule
applies prospectively to every future re-pin.

The canonical truth for "SNIP-side correctness against the chain
wire shape" remains the in-tree fixture + contract test surface
listed above; the local-mirror E2E suite (WS2) is the operator
gate that confirms SNIP and the chain agree end-to-end at this
pin. The in-tree tests are stable, exhaustive for V2 operations,
and run on every PR via the linux CI gate; the WS2 mirror suite
is run before each release.

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
5. Run `make release-check` (linux CI gate). When local-mirror
   E2E is unblocked, run that suite against the new chain commit
   too; until then, fixture + contract tests are the gate.
6. Update [`CHANGELOG.md`](../CHANGELOG.md).
