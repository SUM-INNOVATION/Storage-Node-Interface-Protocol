# L1 RPC reference

The methods SNIP calls on the SUM Chain L1 over JSON-RPC 2.0, with their
parameters and response shapes. This documents the client side of the boundary:
SNIP is a consumer of these methods, and the authoritative definitions live in
the chain repo (`sum-chain/crates/rpc/src/api.rs`). The Rust bindings SNIP uses
are in [`crates/sum-node/src/rpc_client.rs`](../../crates/sum-node/src/rpc_client.rs);
the response structs are in
[`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs).
Wire-format compatibility (bincode-v1 transaction payloads, response shapes) is
pinned by contract tests in both files and must stay byte-aligned with the chain
version noted in [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md).

## Transport and framing

Every call is an HTTP POST to the node's RPC URL (`--rpc-url`, default
`http://127.0.0.1:9944`, the loopback address for a chain node running on the
same host) carrying a JSON-RPC 2.0 request:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "<method>", "params": [ ... ] }
```

A success carries the payload in `result`; a failure carries a JSON-RPC
`error` object. SNIP surfaces any `error` object, and any non-2xx HTTP status,
as a typed client error. Note that "not found" is expressed two different ways
depending on the method: the V1 methods return `null` in `result`, while the V2
`storage_getFileInfoV2` returns a JSON-RPC `error` instead. Each method below
notes which convention it follows.

Method names are not uniformly cased on the wire. The `storage_*` and `chain_*`
methods use camelCase after the prefix (`storage_getFileInfoV2`), while
`send_raw_transaction`, `get_nonce`, and `chain_id` are snake_case. The tables
below give the exact wire string for each.

## Method families

- **[V1 storage](#v1-storage-methods)**: the legacy per-file and per-node
  queries. Still used for V1-registered files and by the MarketSync worker.
- **[Transactions and accounts](#transaction-and-account-methods)**: submit a
  signed transaction, read a nonce or chain ID.
- **[V2 storage lifecycle](#v2-storage-methods)**: the chain-plan-v3.2 methods
  the ingest, push-validation, attestation, and finality paths call.

---

## V1 storage methods

### `storage_getAccessList`

File metadata and access list for one file, by merkle root.

| | |
|---|---|
| **Params** | `[merkle_root_hex]` (0x-prefixed) |
| **Returns** | `StorageFileInfo` or `null` if not registered |
| **Caller** | ACL checks on V1 files |

### `storage_getActiveChallenges`

All open PoR challenges targeting one node.

| | |
|---|---|
| **Params** | `[node_addr_base58]` |
| **Returns** | `[ChallengeInfo]` |
| **Caller** | PorWorker, every `--por-poll-secs` |

### `storage_getFundedFiles`

Every file with a non-zero fee pool (eligible for storage rewards).

| | |
|---|---|
| **Params** | `[]` |
| **Returns** | `[StorageFileInfo]` |
| **Caller** | MarketSync worker (V1-legacy self-heal) |

### `storage_getActiveNodes`

Every active `ArchiveNode`, sorted deterministically by address bytes. This is
the V1 node set used to compute V1 chunk assignment.

| | |
|---|---|
| **Params** | `[]` |
| **Returns** | `[NodeRecordInfo]` |
| **Caller** | V1 assignment, MarketSync |

### `storage_getNodeRecord`

The registry record for one node.

| | |
|---|---|
| **Params** | `[node_addr_base58]` |
| **Returns** | `NodeRecordInfo` or `null` if not registered |

---

## Transaction and account methods

### `send_raw_transaction`

Submit a hex-encoded signed transaction to the mempool. Returns the transaction
hash. The chain has shipped two response shapes across builds and SNIP tolerates
both: a bare hex string `"0xabc..."`, or a wrapped object `{"tx_hash": "0xabc..."}`.
Anything else (a non-string non-object, an object missing `tx_hash`, an object
with a non-string `tx_hash`) is a typed error, so a JSON blob can never be
passed downstream into `chain_getTransactionStatus`.

| | |
|---|---|
| **Params** | `[hex]` (hex-encoded signed tx bytes) |
| **Returns** | tx hash string (extracted from either wire shape) |

### `get_nonce`

Current nonce for an account. Callers set the next transaction's nonce to this
value.

| | |
|---|---|
| **Params** | `[addr_base58]` |
| **Returns** | `u64` |

### `chain_id`

The chain identifier. `register-node` reads this live before signing so the
transaction cannot be mis-flagged against the wrong network. Mainnet is `1`;
the local mirror is `1337`.

| | |
|---|---|
| **Params** | `[]` |
| **Returns** | `u64` |

---

## V2 storage methods

These implement the chain-plan-v3.2 lifecycle. The ingest, push validator,
attestor, and finality waiter call these exclusively.

### `chain_getBlockHeight`

The finalized chain head. SNIP always passes the explicit `"finalized"` param,
because without it the chain defaults to the latest-included height, which makes
safety-critical checks (the V2-enabled gate, abandon grace, reorg windows) racy.

| | |
|---|---|
| **Params** | `["finalized"]` |
| **Returns** | `BlockHeightInfo` |

### `chain_getChainParams`

Live consensus and V2 protocol constants. Read once at startup: the chain treats
these as genesis constants, so a change would be a hard fork. SNIP models only
the fields it consumes; the chain emits additional consensus, fee, and
metadata-cap fields that serde silently ignores.

| | |
|---|---|
| **Params** | `[]` |
| **Returns** | `ChainParamsInfo` |

### `chain_getTransactionStatus`

The V2 finality primitive. Wire shape is internally tagged on `"kind"` with
snake_case variant names. This is the canonical way to wait for a submitted
transaction to finalize (see `tx_wait::wait_for_finalized`).

| | |
|---|---|
| **Params** | `[tx_hash]` |
| **Returns** | `TxStatusV2` |

### `storage_getFileInfoV2`

A V2 file's chain row plus its paginated access list. Unlike the V1 methods,
"file not found" surfaces as a JSON-RPC **error**, not a `null` result. The
last two params paginate the access list; pass `null, null` for the chain
default (offset 0, limit 256).

| | |
|---|---|
| **Params** | `[merkle_root_hex, access_offset, access_limit]` |
| **Returns** | `StorageFileInfoV2` (error if not registered) |

### `storage_getActiveNodesAtHeight`

The active-archive snapshot at a given height. Walks back to the most recent
snapshot at or before `height`; the genesis snapshot at height 0 always exists.
Snapshots are immutable per height, so callers cache aggressively. This is the
input to V2 deterministic chunk assignment, and every participant (uploader,
archives, validators) reads the same snapshot to arrive at the same assignment.

| | |
|---|---|
| **Params** | `[height]` |
| **Returns** | `[NodeRecordInfo]` |

### `account_getEncryptionPublicKey`

The X25519 encryption public key an account has registered (for private files).
Returns `null` when the account has no key, which callers must treat as "this
recipient cannot receive private shares yet" rather than silently dropping them.
The canonical wire shape is a 0x-prefixed 64-char lowercase hex string; SNIP
also tolerates a missing prefix and uppercase hex defensively. A wrong length or
non-hex value is an error, never a silent `null`.

| | |
|---|---|
| **Params** | `[addr_base58]` |
| **Returns** | `0x`-prefixed hex32 string, or `null` |

### `storage_getAssignmentCoverageV2`

Bitmap coverage state for a pending or active V2 file. The uploader polls this
until `can_activate_now` is true, then submits `ActivateFileV2`. `missing_offset`
is a **chunk-index lower bound**, not an offset into the missing list: paginate
by cycling `missing_offset = last_returned_index + 1`. `missing_limit` defaults
to 1024 client-side; the chain caps it at 16384.

| | |
|---|---|
| **Params** | `[merkle_root_hex, missing_offset, missing_limit]` |
| **Returns** | `AssignmentCoverageV2` |

---

## Response types

Field names below are the exact JSON keys. Merkle roots are 0x-prefixed hex,
addresses are base58, and all balances and fees are in Koppa base units
(1 Koppa = 1,000,000,000 base units).

### `StorageFileInfo` (V1)

| Field | Type | Meaning |
|-------|------|---------|
| `merkle_root` | string | 0x-prefixed hex, file identity |
| `owner` | string | base58 L1 address |
| `total_size_bytes` | u64 | file size |
| `access_list` | [string] | base58 addresses allowed to retrieve; empty = public |
| `fee_pool` | u64 | remaining Koppa in the storage fee pool |
| `created_at` | u64 | registration block height |

### `ChallengeInfo`

| Field | Type | Meaning |
|-------|------|---------|
| `challenge_id` | string | 0x-prefixed hex, unique challenge ID |
| `merkle_root` | string | challenged file |
| `chunk_index` | u32 | zero-based chunk challenged |
| `target_node` | string | base58 address of the challenged node |
| `created_at_height` | u64 | issue height |
| `expires_at_height` | u64 | proof deadline (issue + 50 blocks) |

### `NodeRecordInfo`

| Field | Type | Meaning |
|-------|------|---------|
| `address` | string | base58 L1 address |
| `role` | string | `"ArchiveNode"` or `"Validator"` |
| `staked_balance` | u64 | stake in base units |
| `status` | string | `"Active"` or `"Slashed"` |
| `registered_at` | u64 | registration height |

### `BlockHeightInfo`

| Field | Type | Meaning |
|-------|------|---------|
| `height` | u64 | block height |
| `finality` | string | `"finalized"` or `"latest"` |

### `ChainParamsInfo`

Only the fields SNIP consumes are listed; the chain emits more.

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `chain_id` | u64 | | `1` mainnet, `1337` local mirror |
| `block_time_ms` | u64 | | block cadence (2000 on local mirror, 3000 mainnet) |
| `finality_depth` | u64 | | confirmation depths before `Finalized` (6 on mainnet) |
| `min_fee` | u128 | | minimum tx fee, base units |
| `assignment_replication_factor` | u32 | 3 | archives per chunk (R) |
| `max_chunk_indices_per_tx` | u32 | 65536 | cap on chunk indices per `AcceptAssignmentV2` |
| `max_chunk_count_per_file` | u32 | 1048576 | cap on chunks per file |
| `activation_grace_blocks` | u64 | 50 | blocks from register-finalized to earliest abandon / PoR validity |
| `v2_enabled_from_height` | u64? | | height V2 becomes valid; `null` = not activated. SNIP refuses V2 tx submission while `null`, and treats `0` as "enabled from genesis" (distinct from `null`) |

### `TxStatusV2`

Internally tagged on `kind`. Variants:

| `kind` | Payload | Meaning for the finality waiter |
|--------|---------|---------|
| `unknown` | | keep polling |
| `pending` | | keep polling |
| `included` | `block_height` | in a block, not finalized; keep polling |
| `finalized` | `block_height` | done |
| `failed` | `block_height?`, `reason` | terminal for this hash; do not retry the same hash |
| `dropped` | | terminal for this hash; resubmitting the logical op with a fresh nonce is safe |

### `StorageFileInfoV2`

| Field | Type | Meaning |
|-------|------|---------|
| `merkle_root` | string | 0x-prefixed hex |
| `owner` | string | base58 |
| `plaintext_size_bytes` | u64 | file size |
| `stored_size_bytes` | u64 | on-archive bytes (plaintext for public; plaintext + 16 × chunk_count for private) |
| `chunk_count` | u32 | Merkle leaves |
| `fee_pool` | u64 | remaining Koppa |
| `created_at` | u64 | `RegisterFilePendingV2` finalized height |
| `activated_at_height` | u64? | `ActivateFileV2` finalized height; `null` until then |
| `abandoned_at_height` | u64? | `AbandonFileV2` finalized height; `null` if not abandoned or on pre-v3.3 chains |
| `assignment_height` | u64 | snapshot height for chunk assignment |
| `visibility` | u8 | `0` public, `1` private |
| `lifecycle` | u8 | `0` pending, `1` active, `2` abandoned |
| `access_list` | [`AccessEntryV2`] | paginated access entries |

### `AccessEntryV2`

| Field | Type | Meaning |
|-------|------|---------|
| `address` | string | base58 |
| `encrypted_key_bundle` | string? | 0x-prefixed 160-hex-char (80-byte) bundle; `null` for public files |
| `expires_at` | u64? | expiry block height; `null` = no expiry |

### `AssignmentCoverageV2`

| Field | Type | Meaning |
|-------|------|---------|
| `chunk_count` | u32 | total chunks |
| `covered_count` | u32 | popcount over the OR of all active-archive bitmaps |
| `can_activate_now` | bool | `covered_count == chunk_count && lifecycle == Pending` |
| `missing_total` | u32 | total uncovered chunks across the file |
| `missing_offset` | u32 | echoed request lower bound |
| `missing_indices` | [u32] | ascending uncovered indices at or above `missing_offset`, capped at `missing_limit` |
| `per_archive` | [`ArchiveCoverageSummaryV2`] | per-archive summaries |

### `ArchiveCoverageSummaryV2`

| Field | Type | Meaning |
|-------|------|---------|
| `archive` | string | base58 |
| `assigned_count` | u32? | chunks assigned to this archive; `null` for files above the chain's internal per-archive count cap (recompute locally) |
| `attested_count` | u32 | chunks attested via `AcceptAssignmentV2` |
| `currently_active` | bool | whether the archive still counts toward activation |

## See also

- [`CLI.md`](CLI.md): the commands that drive these calls
- [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md): chain version pin and wire-format contract
- [`CMPLT-PROC.md`](CMPLT-PROC.md): how these methods sequence across a file's life
