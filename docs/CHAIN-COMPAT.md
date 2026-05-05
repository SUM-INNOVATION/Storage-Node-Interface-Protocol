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

| Field                      | Value                                          |
|----------------------------|------------------------------------------------|
| Chain repository           | `<internal-chain-repo>`                        |
| Chain release tag          | `<internal-chain-release-tag>`                 |
| Exact chain commit         | provided out-of-band by chain ops              |
| Local-mirror chain id      | `31337`                                        |
| Local-mirror `v2_enabled_from_height` | emits `0` (V2 enabled from genesis) |
| Live-chain `v2_enabled_from_height`   | published with the live release tag |
| Last verified              | `<release-date>`                               |

Re-pinning procedure is in [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md).

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

Local-mirror setup is provided by chain ops out-of-band. Use the
artifact / runbook supplied for the target internal chain release.

## Re-pinning when chain releases a new tag

1. Pull the new chain release tag (out-of-band — chain ops manages
   the private chain repo).
2. Regenerate the bincode-v1 fixtures against the new chain types
   (chain-team responsibility — they own the type definitions
   SNIP mirrors).
3. Run `cargo test -p sum-node tx_builder rpc_client` and
   `cargo test -p sum-types`. Any diff in fixture bytes or
   deserialization tests is an intentional wire-format change and
   MUST land in a separate commit referencing the new chain tag.
4. Update the "Pinned chain version" table at the top of this doc
   with the new release tag and verification date.
5. Run `make release-check` end-to-end against the local mirror.
6. Update [`CHANGELOG.md`](../CHANGELOG.md).
