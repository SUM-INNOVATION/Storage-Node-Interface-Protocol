# Chain integration

SNIP is a client of the SUM Chain. It signs and submits a small,
fixed set of transactions, and it reads a small, fixed set of RPC
methods. This document names the chain-facing feature gates SNIP
depends on, where SNIP discovers each gate at runtime, and what
happens when a gate is off.

Byte-level wire compatibility (bincode-v1 payloads, RPC response
shapes) lives in [`../reference/chain-compat.md`](../reference/chain-compat.md).
The RPC method list lives in [`../reference/rpc-methods.md`](../reference/rpc-methods.md).
This document is one layer up: the semantic gates layered on top of
the wire surface.

## Chain-facing feature gates SNIP reads

The following gates come out of `chain_getChainParams`. Values are
read at runtime; SNIP does not compile in any assumption about them.

| Gate | Where SNIP reads it | What it controls | Off / null behavior | On behavior |
|---|---|---|---|---|
| `v2_enabled_from_height` | `chain_getChainParams` — decoded into `ChainParamsInfo` in [`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs) as `Option<u64>` | Whether V2 op submissions are admitted | `None` — V2 disabled; SNIP refuses V2 tx submission with an "V2 disabled on this chain" error | `Some(N)` — V2 admissible once `finalized_height >= N`. `Some(0)` means enabled from genesis. |
| `assignment_replication_factor` | `chain_getChainParams` | The `R` used by both chain-side and SNIP-side deterministic assignment | — | Mainnet mainnet uses R = 3. |
| `max_chunk_indices_per_tx` | `chain_getChainParams` | Chunk-index list length cap on `AcceptAssignmentV2` | — | Governs batching in the SNIP push wave. |
| `activation_grace_blocks` | `chain_getChainParams` | Height delta an owner must wait past `created_at` before `AbandonFileV2` is admissible | — | Chain plan v3.2 strict `>` rule. |
| `assignment_targeting` | `chain_getChainParams` | Whether V2 PoR challenge targeting uses the chunk's assigned-active archive set (shipped upstream as `sum-chain` issue #97) or the pre-#97 uniform-over-active fallback | Off — challenges may target any active archive for any V2 chunk; archives must be able to fetch on demand within `CHALLENGE_TTL_BLOCKS` | On — challenges restricted to the chunk's assigned-active archive set; unassigned archives are not challenged for that chunk |

**Verify at runtime.** The `assignment_targeting` value on any given
deployment (including mainnet) is not restated as a fixed constant
in SNIP documentation. Operators MUST read it via
`chain_getChainParams` — the value is a property of the chain, not
of SNIP itself. This is the same posture SNIP takes for
`v2_enabled_from_height`.

The full V2 assignment-aware challenge targeting mechanism, its
V1 legacy path, and the separate (planned) bounded coverage
scheduler are described in [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md).

## Chain deployment surface SNIP consumes

- **Transactions SNIP submits** — see [`../reference/chain-compat.md`](../reference/chain-compat.md)
  "Transaction payloads SNIP submits to mainnet." Wire byte layout
  is pinned by fixtures under
  [`crates/sum-node/src/tx_builder.rs`](../../crates/sum-node/src/tx_builder.rs).
- **RPC methods SNIP calls** — see [`../reference/rpc-methods.md`](../reference/rpc-methods.md).
  Deserialization shapes pinned by contract tests under
  [`crates/sum-node/src/rpc_client.rs`](../../crates/sum-node/src/rpc_client.rs)
  `contract_tests` and [`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs).

## Active-archive eligibility contract

Every SNIP consumer of a `storage_getActiveNodesAtHeight` response
narrows the returned
records to exactly-eligible archives before any address decode. The
contract is:

```
role == "ArchiveNode" && status == "Active"
```

matched byte-for-byte against the strings the chain produces via
`format!("{:?}", NodeRole)` / `format!("{:?}", NodeStatus)`.
Records with role `"Validator"` or with status `"Slashed"`,
`"Unbonding"`, `"Withdrawn"`, or any future unknown-status string
are ineligible.

Implemented as `NodeRecordInfo::is_active_archive(&self) -> bool`
and the shared filter helper `filter_active_archives(...)` in
[`crates/sum-types/src/rpc_types.rs`](../../crates/sum-types/src/rpc_types.rs).

**Consumer audit** — the filter is applied at every SNIP site that
treats an active-nodes response as assignment-eligible input:

| Consumer | Site |
|---|---|
| V1 upload orchestrator | `crates/sum-node/src/upload.rs::run` |
| V1 download holder map | `crates/sum-node/src/download.rs::build_holder_map` |
| MarketSync (V1 self-heal) + GC retained-set | `crates/sum-node/src/market_sync.rs::sync_cycle` |
| V2 ingest / resume snapshot | `crates/sum-node/src/ingest_v2.rs::fetch_assignment_inputs` and `run_resume_v2` snapshot |
| V2 push validator admission cache | `crates/sum-node/src/push_validator.rs::fetch_snapshot` |
| V2 public routing | `crates/sum-node/src/download_v2_routing.rs::build_v2_assignment_view` |
| V2 private manifest routing | `crates/sum-node/src/download_private.rs` (manifest fetch + chunk fanout) |
| V2 inbound attest trigger | `crates/sum-node/src/inbound_v2.rs::AttestTriggerRpc::fetch_snapshot` |
| Operator readiness gate | `crates/sum-node/src/bin/e2e_helper.rs::build_active_nodes_report` — uses `is_active_archive` for `--require-archives`, keeps role×status tally for operator visibility |

The receive-side V2 attestor (`assignment_attestor.rs`) inherits an
already-filtered snapshot from the dispatcher above and does not
apply an additional filter.

## What SNIP does not read

SNIP does not depend on `sum-chain`-internal storage layouts (CF
column-family names, on-disk key encodings, RocksDB paths, etc.).
Documentation that references those layouts by symbol name is
usually an archived planning document; treat it as historical.

## Cross-references

- V2 lifecycle state machine SNIP drives: [`../protocol/v2-state-machine.md`](../protocol/v2-state-machine.md).
- V2 PoR targeting and the `assignment_targeting` gate: [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md).
- Deep wire compatibility, chain pin, mainnet-vs-mirror deltas: [`../reference/chain-compat.md`](../reference/chain-compat.md).
- CLI flags whose semantics reference the gates above: [`../reference/config-flags.md`](../reference/config-flags.md).
