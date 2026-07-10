# SUM Chain JSON-RPC methods SNIP uses

SNIP interacts with the SUM Chain over JSON-RPC. This is the
exhaustive list of methods SNIP calls. Anything not on this list
is either not exercised in the current build or is a
diagnostic-only helper not shipped in production paths.

Deserialization shapes are pinned by the contract tests named in
[`chain-compat.md`](chain-compat.md) "RPC contract tests." A diff
in any of those shapes is a chain-compat break and requires
coordination with the chain team.

## Reads

| Method | Purpose | Called from |
|---|---|---|
| `chain_getChainParams` | Read `chain_id`, `v2_enabled_from_height`, `assignment_replication_factor`, `max_chunk_indices_per_tx`, `activation_grace_blocks`, `assignment_targeting`, other tunables | Ingest preflight; smoke gate; feature-gate discovery |
| `chain_getBlockHeight` | Read finalized head | V2 enablement gate; abandon grace check; finality budgets |
| `chain_getTransactionStatus` | Poll a submitted tx to `Finalized` / `Failed` / `Dropped` | Every tx-submitting path after `send_raw_transaction` |
| `get_balance` | Preflight: does this address have enough Koppa? | Runbook smoke / manual preflight |
| `get_nonce` | Fetch nonce for signing the next tx | Every tx-signing path |
| `chain_id` | Live-RPC chain id read | `register-node`; used to be safe against `--chain-id` misconfiguration |
| `storage_getFileInfoV2` | Read a V2 file's row: visibility, lifecycle, access list, fee pool | Route Public vs Private; verify preconditions before write; ACL check on serve |
| `storage_getActiveNodesAtHeight` | Snapshot of active-`ArchiveNode` rows at a chain height | V2 ingest assignment; three-archive bootstrap gate |
| `storage_getAssignmentCoverageV2` | Coverage bitmap + `can_activate_now` predicate for a Pending V2 file | Ingest S4 coverage poll |
| `storage_getActiveNodes` | Snapshot of active archives at the finalized head (V1) | V1 MarketSync path |
| `storage_getFundedFiles` | V1 files with `fee_pool > 0` | V1 MarketSync path |
| `storage_getActiveChallenges` | Pending PoR challenges targeting a specific address | `PorWorker` background loop |
| `account_getEncryptionPublicKey` | Resolve a recipient's registered X25519 pubkey | Private V2 ingest; `share` preflight |
| `storage_getAccessList` (paginated) | Read a file's access list, `AccessEntryV2` at a time | Private V2 download; ACL check on serve |
| `health` | Chain-side liveness probe | `e2e-helper health` |

## Writes

Every write path signs a bincode-v1 transaction and submits via
`send_raw_transaction`. See
[`chain-compat.md`](chain-compat.md) "Transaction payload fixtures"
for the pinned byte shapes; each is exhaustively fixture-tested in
[`crates/sum-node/src/tx_builder.rs`](../../crates/sum-node/src/tx_builder.rs).

| Method | Called from |
|---|---|
| `send_raw_transaction` | Every SNIP write path |

Transactions SNIP submits:

- `TxPayload::NodeRegistry(Register { role: ArchiveNode, stake })` — `sum-node register-node` / `e2e-helper register-node`
- `TxPayload::NodeRegistryV2(RegisterEncryptionKey)` — `sum-node register-encryption-key`
- `TxPayload::StorageMetadata(SubmitStorageProof { ... })` — `PorWorker` responder
- `TxPayload::StorageMetadataV2(RegisterFilePendingV2)` — `ingest-v2`
- `TxPayload::StorageMetadataV2(ActivateFileV2)` — `ingest-v2` S5, `resume` residual
- `TxPayload::StorageMetadataV2(AbandonFileV2)` — `abandon`
- `TxPayload::StorageMetadataV2(AcceptAssignmentV2)` — `AssignmentAttestor` background
- `TxPayload::StorageMetadataV2(AddAccessV2)` — `share`
- `TxPayload::StorageMetadataV2(RemoveAccessV2)` — `revoke`
- `TxPayload::StorageMetadataV2(UpdateAccessV2)` — `update-access`

## Methods SNIP intentionally does NOT use

The mainnet chain exposes alias methods (`sum_sendRawTransaction`
and per-tx receipt aliases). SNIP does **not** use them. Staying
on `send_raw_transaction` + `chain_getTransactionStatus` keeps the
wire surface SNIP depends on small and review-able; an alias
divergence on the chain side is a no-op for SNIP.

## Cross-references

- Wire compatibility surface, chain pin, mainnet-vs-mirror deltas:
  [`chain-compat.md`](chain-compat.md).
- Chain-facing feature gates SNIP reads from these RPC methods:
  [`../architecture/chain-integration.md`](../architecture/chain-integration.md).
- CLI surface that consumes these methods: [`cli.md`](cli.md).
