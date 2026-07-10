//! RPC response types matching the SUM Chain L1's JSON-RPC output.
//!
//! These structs deserialize directly from the JSON shapes produced by
//! `sum-chain/crates/rpc/src/server.rs` (lines 4912-5012).

use serde::{Deserialize, Serialize};

/// File metadata returned by `storage_getAccessList` and `storage_getFundedFiles`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFileInfo {
    /// Merkle root as 0x-prefixed hex string (64 hex chars + "0x").
    pub merkle_root: String,
    /// File owner's L1 address in base58 format.
    pub owner: String,
    /// Total file size in bytes.
    pub total_size_bytes: u64,
    /// List of L1 addresses (base58) allowed to retrieve this file.
    /// Empty means the file is public.
    pub access_list: Vec<String>,
    /// Remaining Koppa in the storage fee pool (base units).
    pub fee_pool: u64,
    /// Block height at which the file was registered.
    pub created_at: u64,
}

/// Active challenge returned by `storage_getActiveChallenges`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeInfo {
    /// Unique challenge identifier (0x-prefixed hex).
    pub challenge_id: String,
    /// Challenged file's merkle root (0x-prefixed hex).
    pub merkle_root: String,
    /// Zero-based chunk index within the file.
    pub chunk_index: u32,
    /// L1 address (base58) of the node being challenged.
    pub target_node: String,
    /// Block height at which the challenge was issued.
    pub created_at_height: u64,
    /// Block height by which the proof must be submitted (created + 50 blocks).
    pub expires_at_height: u64,
}

/// Node record returned by `storage_getNodeRecord` and every entry of
/// `storage_getActiveNodes` / `storage_getActiveNodesAtHeight`.
///
/// The chain formats `role` and `status` via `format!("{:?}", …)` over
/// its `NodeRole` / `NodeStatus` enums (upstream
/// `sum-chain/crates/rpc/src/server.rs:7595-7839`,
/// `sum-chain/crates/primitives/src/node_registry.rs`), producing the
/// strings compared by [`NodeRecordInfo::is_active_archive`] and
/// [`NodeRecordInfo::known_status`]. Fields remain typed as `String`
/// so unknown future variants added on the chain side deserialize
/// without error and remain observable through the raw string —
/// consumers that need eligibility use the exact-match helpers below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecordInfo {
    /// Node's L1 address in base58.
    pub address: String,
    /// Role string — `"ArchiveNode"` and `"Validator"` today (chain's
    /// `NodeRole` enum has these two variants); unknown-future values
    /// pass through as the raw string and are treated as ineligible.
    pub role: String,
    /// Staked balance in Koppa base units.
    pub staked_balance: u64,
    /// Status string — `"Active"`, `"Slashed"`, `"Unbonding"`, and
    /// `"Withdrawn"` are the four current chain variants; unknown-future
    /// values pass through as the raw string and are treated as
    /// ineligible.
    pub status: String,
    /// Block height at which the node registered.
    pub registered_at: u64,
}

impl NodeRecordInfo {
    /// The SNIP-wide eligibility contract for including this record in
    /// a V1 or V2 chunk-assignment set, a push admission cache, or an
    /// operator readiness gate.
    ///
    /// Returns `true` iff `role == "ArchiveNode"` **and**
    /// `status == "Active"`. Any other combination — Validator,
    /// Unbonding, Withdrawn, Slashed, or any unknown-future value —
    /// returns `false`.
    ///
    /// Every SNIP consumer of `Vec<NodeRecordInfo>` that treats input
    /// as "assignment-eligible archives" MUST filter through this
    /// helper (or equivalently through [`filter_active_archives`])
    /// before any address decode or assignment computation.
    pub fn is_active_archive(&self) -> bool {
        self.role == "ArchiveNode" && self.status == "Active"
    }

    /// `Some` for a status the workspace recognises at this release;
    /// `None` for any future chain-added status not yet enumerated on
    /// SNIP's side. Callers that need to display the raw string keep
    /// reading [`NodeRecordInfo::status`].
    pub fn known_status(&self) -> Option<KnownNodeStatus> {
        match self.status.as_str() {
            "Active" => Some(KnownNodeStatus::Active),
            "Slashed" => Some(KnownNodeStatus::Slashed),
            "Unbonding" => Some(KnownNodeStatus::Unbonding),
            "Withdrawn" => Some(KnownNodeStatus::Withdrawn),
            _ => None,
        }
    }
}

/// The four `NodeStatus` variants the chain currently emits (upstream
/// `sum-chain/crates/primitives/src/node_registry.rs`). Callers that
/// receive a future chain-added status observe [`NodeRecordInfo::status`]
/// directly; there is no closed-enum wire representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownNodeStatus {
    Active,
    Slashed,
    Unbonding,
    Withdrawn,
}

/// Narrow a `storage_getActiveNodes[AtHeight]` response to exactly-
/// eligible archives per [`NodeRecordInfo::is_active_archive`]. Preserves
/// order and does NOT dedupe (callers that need dedupe do it after
/// address decode).
///
/// This is the workspace's single point of eligibility filtering. Every
/// consumer of a snapshot response that treats input as assignment-
/// eligible archives applies this helper before address decode; the
/// consumer audit is recorded in `docs/reference/chain-compat.md`.
pub fn filter_active_archives(records: Vec<NodeRecordInfo>) -> Vec<NodeRecordInfo> {
    records
        .into_iter()
        .filter(NodeRecordInfo::is_active_archive)
        .collect()
}

// ── V2 RPC types (chain plan v3.2) ───────────────────────────────────────────
//
// These mirror the chain plan §4 wire shapes. Field naming follows the
// existing V1 convention (snake_case, hex-encoded merkle roots, base58
// addresses, lifecycle/visibility surfaced as integers per §4 line 470).
//
// The chain plan uses Rust syntax in §4 to describe the responses; the
// actual JSON-RPC wire goes through `serde_json` so we choose
// representations that round-trip cleanly: `LifecycleV2` and
// `VisibilityV2` are `#[serde(transparent)]` newtypes over `u8`,
// `TxStatusV2` is an internally-tagged enum keyed on `"kind"` with
// snake_case variant names — chain-confirmed wire shape.

/// File lifecycle state on chain. Wire format: a plain `u8`.
///
/// Constants are defined to match chain plan §3.1: `Pending = 0`,
/// `Active = 1`, `Abandoned = 2`. Future variants (e.g. `Rotated = 3`
/// from chain plan Ask 10) would extend this without breaking the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct LifecycleV2(pub u8);

impl LifecycleV2 {
    pub const PENDING: Self = Self(0);
    pub const ACTIVE: Self = Self(1);
    pub const ABANDONED: Self = Self(2);

    pub fn is_pending(self) -> bool {
        self == Self::PENDING
    }
    pub fn is_active(self) -> bool {
        self == Self::ACTIVE
    }
    pub fn is_abandoned(self) -> bool {
        self == Self::ABANDONED
    }
}

/// File visibility on chain. Wire format: a plain `u8`.
///
/// `Public = 0`, `Private = 1`. Phase 0b is Public-only; Private wiring
/// lands in Phase 4 once `sum-crypto` is plumbed into the ingest path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct VisibilityV2(pub u8);

impl VisibilityV2 {
    pub const PUBLIC: Self = Self(0);
    pub const PRIVATE: Self = Self(1);

    pub fn is_public(self) -> bool {
        self == Self::PUBLIC
    }
    pub fn is_private(self) -> bool {
        self == Self::PRIVATE
    }
}

/// One entry of a V2 access list (chain plan §3.1).
///
/// For Public files all `encrypted_key_bundle` fields MUST be `None`;
/// for Private files all MUST be `Some(160-hex-char-string)`. Owner
/// always present in the list. `expires_at` is a block height; `None`
/// = no expiry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessEntryV2 {
    /// L1 address in base58 (matches the V1 `StorageFileInfo.access_list` element type).
    pub address: String,
    /// 80-byte encrypted key bundle as a 0x-prefixed hex string (162 chars).
    /// `None` for Public files.
    pub encrypted_key_bundle: Option<String>,
    /// Expiry block height; `None` = no expiry.
    pub expires_at: Option<u64>,
}

/// Response from `storage_getFileInfoV2` (chain plan §4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageFileInfoV2 {
    /// 0x-prefixed hex (64 hex chars + "0x").
    pub merkle_root: String,
    /// Owner's L1 address in base58.
    pub owner: String,
    /// Plaintext file size in bytes.
    pub plaintext_size_bytes: u64,
    /// Total bytes stored on archive nodes (= plaintext for Public,
    /// plaintext + 16 × chunk_count for Private).
    pub stored_size_bytes: u64,
    /// Number of leaves in the file's Merkle tree. Bounded above by the
    /// chain's `max_chunk_count_per_file` (default 1_048_576, §3.4).
    pub chunk_count: u32,
    /// Remaining Koppa in the storage fee pool (base units).
    pub fee_pool: u64,
    /// Block height at which `RegisterFilePendingV2` was finalized.
    pub created_at: u64,
    /// Block height at which `ActivateFileV2` finalized; `None` until then.
    pub activated_at_height: Option<u64>,
    /// Block height at which `AbandonFileV2` finalized; `None` for files
    /// still in `Pending`/`Active` lifecycle.
    ///
    /// **Chain version gating:** this field requires chain v3.3+ (post
    /// W11 follow-up binary). Earlier chain versions don't populate the
    /// underlying `StorageMetadataV2.abandoned_at_height` field, so a
    /// V2 file row deserialized from a pre-v3.3 chain may carry `None`
    /// even when `lifecycle == Abandoned`. Callers MUST treat `None`
    /// defensively as "abandoned-height unavailable" rather than as a
    /// retryable error — chain plan §3.5 doesn't expose a separate
    /// "was abandoned at" timestamp for archival auditing pre-v3.3.
    #[serde(default)]
    pub abandoned_at_height: Option<u64>,
    /// Block height of the active-archive-node snapshot used for this file's
    /// chunk assignment (chain plan §5.3, Ask 15 Option A).
    pub assignment_height: u64,
    pub visibility: VisibilityV2,
    pub lifecycle: LifecycleV2,
    /// Paginated access list entries. Pagination params: `access_offset`,
    /// `access_limit` (default 256). Phase 0b's typical Public files have
    /// empty access lists, so pagination rarely matters in practice.
    pub access_list: Vec<AccessEntryV2>,
}

/// Response from `chain_getBlockHeight` (chain plan §4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockHeightInfo {
    pub height: u64,
    /// `"finalized"` (depth ≥ N for the active consensus mode) or
    /// `"latest"` (most recent included).
    pub finality: String,
}

/// Response from `chain_getChainParams` (chain-confirmed wire shape).
///
/// Flat JSON shape (no nested `staking`/`messaging`/`docclass`
/// sub-objects). Models the fields SNIP actively consumes — the chain
/// emits additional consensus / fee / metadata-cap fields that SNIP
/// doesn't need (`max_block_bytes`, `max_txs_per_block`,
/// `storage_fee_per_byte`, `max_metadata_bytes`, `max_access_list_bytes`,
/// `abandonment_fee_percent`); serde silently ignores those, keeping
/// the struct minimal.
///
/// **Removed in this revision**: `max_assigned_count_chunk_count`. The
/// chain does NOT emit this field; the per-archive count cap is
/// detected at runtime via `assigned_count: None` in
/// `AssignmentCoverageV2`. Earlier SNIP revisions made it required,
/// which silently broke `chain_getChainParams` deserialization.
///
/// **Added in this revision**: `v2_enabled_from_height`. SNIP must
/// gate every V2 lifecycle tx (RegisterFilePendingV2,
/// AcceptAssignmentV2, ActivateFileV2, AbandonFileV2,
/// RegisterEncryptionKey) on `current_block_height >=
/// v2_enabled_from_height` to avoid burning fees against a chain that
/// hasn't activated V2 yet.
///
/// **Always read this once at startup** — chain treats these as
/// protocol constants per chain genesis, not per-block values. A
/// change would be a hard fork.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainParamsInfo {
    /// Chain identifier (mainnet / testnet / local-mirror = 31337).
    pub chain_id: u64,
    /// Block production cadence (e.g. 2,000 ms for the local mirror).
    pub block_time_ms: u64,
    /// Number of confirmation depths required before a tx is treated as
    /// `Finalized` (PoA: 3; BFT: 0).
    pub finality_depth: u64,
    /// Minimum fee per tx in Koppa base units. Tx builders must set
    /// `tx.fee >= min_fee`.
    pub min_fee: u128,
    /// V2 deterministic-assignment replication factor — the number of
    /// archives each chunk is assigned to. Default = 3.
    pub assignment_replication_factor: u32,
    /// Cap on `chunk_indices.len()` per `AcceptAssignmentV2` tx. Default
    /// = 65,536. Files whose per-archive assignment exceeds this split
    /// across multiple OR-merge txs.
    pub max_chunk_indices_per_tx: u32,
    /// Cap on `chunk_count` per file. Default = 1,048,576 (1 MiB chunks
    /// × 2^20 = 1 TiB max plaintext).
    pub max_chunk_count_per_file: u32,
    /// Blocks between `RegisterFilePendingV2` finalization and the
    /// earliest height at which `AbandonFileV2` can succeed (and at
    /// which post-activation PoR challenges become valid). Default = 50.
    /// **Not** a pre-activation coverage timeout; W10 uses a wall-clock
    /// `activation_wait_secs` for that.
    pub activation_grace_blocks: u64,
    /// Block height at which the V2 storage lifecycle becomes valid.
    /// Tx submission for V2 ops MUST verify
    /// `Some(h)` where `current_height >= h`, or the tx will be
    /// rejected and fees burned.
    ///
    /// `Option<u64>` so all three chain-emission shapes deserialize:
    ///   - field missing → `None` via `#[serde(default)]`
    ///   - explicit `null` → `None`
    ///   - explicit `u64` → `Some(n)`
    ///
    /// Semantically, `None` means "no activation height set on chain"
    /// (chain hasn't planned a V2 cutover, or is too old to know about
    /// V2 at all). SNIP must NOT treat `None` as "enabled from
    /// genesis" — a sane gate refuses V2 tx submission until the
    /// chain advertises a concrete number.
    #[serde(default)]
    pub v2_enabled_from_height: Option<u64>,
}

/// Response from `chain_getTransactionStatus` (chain plan §4,
/// chain-confirmed wire shape).
///
/// **Wire shape**: internally-tagged JSON with `"kind"` key and
/// snake_case variant names — `{"kind": "pending"}`,
/// `{"kind": "included", "block_height": 123}`,
/// `{"kind": "failed", "block_height": null, "reason": "..."}`, etc.
///
/// Semantics for the SNIP tx finality waiter (W9):
/// * `Unknown` — keep polling; not fatal during the wait window.
/// * `Pending` — keep polling.
/// * `Included` — keep polling; not yet finalized.
/// * `Finalized` — done; return the block height.
/// * `Failed` — terminal for THIS tx hash. Caller must NOT retry the
///   same hash. May rebuild a new tx with a fresh nonce only if the
///   operation is idempotent and the failure is classified as transient.
/// * `Dropped` — terminal for THIS tx hash, but resubmitting the same
///   logical operation (with a new nonce) is safe since the tx never
///   touched chain state.
/// Wire shape: chain emits `{"kind": "<variant>", ...payload-fields}`
/// with `kind` lower_snake_case. Example:
///   `{"kind": "finalized", "block_height": 123}`
///   `{"kind": "failed", "block_height": null, "reason": "fee_too_low"}`
///   `{"kind": "pending"}`
/// Earlier SNIP code keyed this on `"status"` — that drops every
/// real-chain status response on the floor as a deserialization error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxStatusV2 {
    Unknown,
    Pending,
    Included {
        block_height: u64,
    },
    Finalized {
        block_height: u64,
    },
    Failed {
        block_height: Option<u64>,
        reason: String,
    },
    Dropped,
}

/// Per-archive coverage summary inside `AssignmentCoverageV2`.
///
/// `attested_count` is a popcount over the archive's attestation
/// bitmap row; the raw bitmap bytes are NEVER serialized in the RPC
/// response (chain plan §4 line 474).
///
/// `assigned_count` is `Option<u32>` (JSON nullable) by chain design.
/// The chain caps per-archive assignment-count work at an internal
/// threshold (currently 16,384 chunks) and elides the count for files
/// above that cap; the SNIP client treats `None` as "recompute
/// locally" via `sum_store::assignment_v2::chunks_for_archive_v2`
/// (which is bit-identical to chain validation per Appendix C). The
/// cap value itself is **not** exposed via `chain_getChainParams` — do
/// not try to read it. Coverage fields (`covered_count`,
/// `missing_indices`, `can_activate_now`) are always populated
/// regardless.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveCoverageSummaryV2 {
    /// L1 address in base58.
    pub archive: String,
    /// Number of chunks the deterministic V2 assignment puts on this
    /// archive. `None` when chain elides the count for files above
    /// the chain's internal per-archive count cap (currently 16,384).
    pub assigned_count: Option<u32>,
    /// Number of chunks this archive has attested via `AcceptAssignmentV2`.
    pub attested_count: u32,
    /// Whether the archive is currently `Active` (still counts toward
    /// `ActivateFileV2` validity). A `Slashed`/`Inactive` archive's
    /// attestations are dropped from the coverage popcount.
    pub currently_active: bool,
}

/// Response from `storage_getAssignmentCoverageV2` (chain plan v3.2 §4).
///
/// Owner polls until `can_activate_now == true`, then submits
/// `ActivateFileV2`. `missing_offset` is a **chunk-index lower bound**,
/// not a list offset — pagination cycles `missing_offset =
/// last_returned_index + 1` and is stable under concurrent attestations.
///
/// Post-#62 (upstream `sum-chain` issue #62): the response gained
/// `assignment_epochs`, `latest_assignment_epoch`, `reassignment_needed`,
/// and `per_epoch`. All four are `#[serde(default)]` so legacy pre-#62
/// chain responses (which omit them) deserialize cleanly.
/// `top-level `per_archive` is now epoch-0-only per upstream contract
/// (upstream `crates/state/src/storage_metadata.rs:2190-2194`);
/// reassignment-aware clients should read `per_epoch` for later epochs.
///
/// Top-level `covered_count`, `missing_total`, and `missing_indices`
/// remain **aggregate** — the OR of all epochs' snapshot-active
/// attestation bitmaps (upstream `storage_metadata.rs:2181-2200`),
/// so existing SNIP consumers (`s4_wait_coverage`, `collect_missing_indices`)
/// continue to work correctly under multi-epoch responses without a
/// behavior change. `can_activate_now` gates on aggregate coverage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignmentCoverageV2 {
    pub chunk_count: u32,
    /// Popcount over the OR of all snapshot-active archive bitmaps
    /// **aggregated across every assignment epoch** (post-#62).
    pub covered_count: u32,
    /// `covered_count == chunk_count && lifecycle == Pending`. Chain
    /// gates `ActivateFileV2` on the aggregate condition, so this
    /// remains meaningful under multi-epoch responses.
    pub can_activate_now: bool,
    /// Total uncovered chunks across the whole file (not just the
    /// window). Aggregate across all epochs.
    pub missing_total: u32,
    /// Echoed back from the request (or 0 if not specified).
    pub missing_offset: u32,
    /// Ascending list of `i ≥ missing_offset` where the aggregate
    /// coverage bitmap has 0. Capped at `missing_limit` (default 1024;
    /// W9 client passes 1024). Aggregate across all epochs.
    pub missing_indices: Vec<u32>,
    /// Per-archive popcount summaries, **epoch-0-only** post-#62.
    /// Reassignment-aware clients read `per_epoch` for later epochs.
    pub per_archive: Vec<ArchiveCoverageSummaryV2>,
    /// All assignment epoch heights, ascending. Chain guarantees
    /// (verified upstream in
    /// `sum-chain/crates/state/src/storage_metadata.rs:2178-2179`):
    /// - Non-empty when present in the wire response
    ///   (epoch 0 always present).
    /// - `[0] == StorageMetadataV2.assignment_height`.
    /// - `last() == latest_assignment_epoch`.
    ///
    /// Pre-#62 chain: field absent, deserializes as empty via
    /// `#[serde(default)]`. Consumers use
    /// [`AssignmentCoverageV2::resolved_latest_epoch`] rather than
    /// reading `.last()` directly.
    #[serde(default)]
    pub assignment_epochs: Vec<u64>,
    /// == `assignment_epochs.last()` when present. Pre-#62 chain:
    /// absent → 0 via `#[serde(default)]`. Because 0 is a valid epoch
    /// height, consumers MUST NOT compare this to 0 as a legacy
    /// sentinel — use
    /// [`AssignmentCoverageV2::resolved_latest_epoch`].
    #[serde(default)]
    pub latest_assignment_epoch: u64,
    /// True iff an archive assigned under the latest epoch has left
    /// the active set — chain accepts `ReassignChunksV2` iff this is
    /// true. Pre-#62 chain: `false` via `#[serde(default)]`. Safe
    /// legacy default: pre-#62 chains have no reassignment concept,
    /// and aggregate `can_activate_now` semantics do not depend on
    /// this field.
    #[serde(default)]
    pub reassignment_needed: bool,
    /// Per-epoch coverage detail. Chain guarantees
    /// `per_epoch.len() == assignment_epochs.len()` and same order
    /// (upstream `storage_metadata.rs:2183-2188`). Pre-#62 chain:
    /// empty via `#[serde(default)]`. Reassignment-aware consumers
    /// read this instead of top-level epoch-0-only `per_archive` when
    /// they need per-epoch detail.
    #[serde(default)]
    pub per_epoch: Vec<AssignmentEpochCoverageV2>,
}

impl AssignmentCoverageV2 {
    /// The latest assignment epoch height, or `None` for a pre-#62
    /// chain response (empty `assignment_epochs`).
    ///
    /// **Cannot detect malformed partial metadata.** With
    /// `#[serde(default)]` the representation cannot distinguish
    /// "field absent" from "field explicitly empty/zero/false".
    /// SNIP trusts the upstream invariant that a chain emitting
    /// `assignment_epochs` emits a non-empty vec whose last entry
    /// equals `latest_assignment_epoch` (upstream
    /// `storage_metadata.rs:2178-2179`). If a chain regresses this
    /// invariant, `resolved_latest_epoch` will misclassify the
    /// response as legacy. Option-wrapped inner fields would let
    /// callers distinguish these states but would change the public
    /// field types.
    pub fn resolved_latest_epoch(&self) -> Option<u64> {
        if self.assignment_epochs.is_empty() {
            None
        } else {
            Some(self.latest_assignment_epoch)
        }
    }
}

/// One epoch's coverage detail within `AssignmentCoverageV2.per_epoch`
/// (upstream `sum-chain` issue #62). `epoch_height` names the
/// active-archive snapshot used for this epoch's assignment; query
/// `storage_getActiveNodesAtHeight(epoch_height)` to re-derive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignmentEpochCoverageV2 {
    /// Block height of this epoch's assignment snapshot.
    pub epoch_height: u64,
    /// True for epoch 0 (the file's original `assignment_height`).
    pub is_epoch_zero: bool,
    /// Chunks covered by currently-Active attesters **in this epoch**
    /// only.
    pub covered_count: u32,
    /// Per-archive summary for this epoch's snapshot; `assigned_count` /
    /// `attested_count` are both scoped to this epoch.
    pub per_archive: Vec<ArchiveCoverageSummaryV2>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_file_info_deserialize() {
        let json = r#"{
            "merkle_root": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            "owner": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "total_size_bytes": 10485760,
            "access_list": ["DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q"],
            "fee_pool": 100000000000,
            "created_at": 12345
        }"#;
        let info: StorageFileInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.total_size_bytes, 10_485_760);
        assert_eq!(info.access_list.len(), 1);
        assert_eq!(info.fee_pool, 100_000_000_000);
    }

    #[test]
    fn challenge_info_deserialize() {
        let json = r#"{
            "challenge_id": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "merkle_root": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "chunk_index": 5,
            "target_node": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "created_at_height": 1000,
            "expires_at_height": 1050
        }"#;
        let info: ChallengeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.chunk_index, 5);
        assert_eq!(info.expires_at_height, 1050);
    }

    #[test]
    fn node_record_info_deserialize() {
        let json = r#"{
            "address": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "role": "ArchiveNode",
            "staked_balance": 5000000000000,
            "status": "Active",
            "registered_at": 12300
        }"#;
        let info: NodeRecordInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.role, "ArchiveNode");
        assert_eq!(info.status, "Active");
    }

    #[test]
    fn storage_file_info_empty_access_list() {
        let json = r#"{
            "merkle_root": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "owner": "test",
            "total_size_bytes": 1048576,
            "access_list": [],
            "fee_pool": 0,
            "created_at": 1
        }"#;
        let info: StorageFileInfo = serde_json::from_str(json).unwrap();
        assert!(info.access_list.is_empty());
    }

    // ── V2 type round-trips (chain plan v3.2 §4) ─────────────────────────────

    #[test]
    fn lifecycle_visibility_serialize_as_u8() {
        // Wire shape is a plain integer (not a string variant).
        assert_eq!(serde_json::to_string(&LifecycleV2::PENDING).unwrap(), "0");
        assert_eq!(serde_json::to_string(&LifecycleV2::ACTIVE).unwrap(), "1");
        assert_eq!(serde_json::to_string(&LifecycleV2::ABANDONED).unwrap(), "2");
        assert_eq!(serde_json::to_string(&VisibilityV2::PUBLIC).unwrap(), "0");
        assert_eq!(serde_json::to_string(&VisibilityV2::PRIVATE).unwrap(), "1");

        // And the helper predicates round-trip.
        let lc: LifecycleV2 = serde_json::from_str("1").unwrap();
        assert!(lc.is_active());
        assert!(!lc.is_pending());
        let v: VisibilityV2 = serde_json::from_str("0").unwrap();
        assert!(v.is_public());
    }

    #[test]
    fn storage_file_info_v2_public_round_trip() {
        // A typical Phase 0b Public file: empty access_list, lifecycle Pending.
        let json = r#"{
            "merkle_root": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            "owner": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "plaintext_size_bytes": 10485760,
            "stored_size_bytes": 10485760,
            "chunk_count": 10,
            "fee_pool": 100000000000,
            "created_at": 12345,
            "activated_at_height": null,
            "assignment_height": 12345,
            "visibility": 0,
            "lifecycle": 0,
            "access_list": []
        }"#;
        let info: StorageFileInfoV2 = serde_json::from_str(json).unwrap();
        assert_eq!(info.chunk_count, 10);
        assert_eq!(info.plaintext_size_bytes, info.stored_size_bytes);
        assert!(info.visibility.is_public());
        assert!(info.lifecycle.is_pending());
        assert!(info.activated_at_height.is_none());
        assert!(info.access_list.is_empty());
    }

    #[test]
    fn storage_file_info_v2_private_with_access_entries() {
        // Private file: stored_size = plaintext_size + 16 * chunk_count;
        // access_list entries carry encrypted_key_bundle (160 hex + "0x").
        let bundle_hex = format!("0x{}", "ab".repeat(80)); // 160 hex chars
        let json = format!(
            r#"{{
                "merkle_root": "0x{root}",
                "owner": "ownerB58",
                "plaintext_size_bytes": 1000000,
                "stored_size_bytes": 1000160,
                "chunk_count": 10,
                "fee_pool": 0,
                "created_at": 100,
                "activated_at_height": 150,
                "assignment_height": 100,
                "visibility": 1,
                "lifecycle": 1,
                "access_list": [
                    {{
                        "address": "ownerB58",
                        "encrypted_key_bundle": "{bundle_hex}",
                        "expires_at": null
                    }},
                    {{
                        "address": "recipientB58",
                        "encrypted_key_bundle": "{bundle_hex}",
                        "expires_at": 200
                    }}
                ]
            }}"#,
            root = "11".repeat(32),
            bundle_hex = bundle_hex,
        );
        let info: StorageFileInfoV2 = serde_json::from_str(&json).unwrap();
        assert!(info.visibility.is_private());
        assert!(info.lifecycle.is_active());
        assert_eq!(info.activated_at_height, Some(150));
        assert_eq!(info.stored_size_bytes - info.plaintext_size_bytes, 16 * 10);
        assert_eq!(info.access_list.len(), 2);
        assert!(info.access_list[0].encrypted_key_bundle.is_some());
        assert!(info.access_list[0].expires_at.is_none()); // owner: no expiry
        assert_eq!(info.access_list[1].expires_at, Some(200));
    }

    /// `abandoned_at_height` requires chain v3.3+. Pre-v3.3 chains
    /// don't include the field at all → `Option::None` via
    /// `#[serde(default)]`. Post-v3.3 chains include it as either
    /// `null` (file not yet abandoned) or a `u64` (block height of the
    /// finalized `AbandonFileV2`).
    #[test]
    fn storage_file_info_v2_abandoned_at_height_pre_and_post_v3_3() {
        // Pre-v3.3 chain: field omitted entirely. Must default to None.
        let pre_v3_3 = r#"{
            "merkle_root": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "owner": "ownerB58",
            "plaintext_size_bytes": 0,
            "stored_size_bytes": 0,
            "chunk_count": 1,
            "fee_pool": 0,
            "created_at": 100,
            "activated_at_height": null,
            "assignment_height": 100,
            "visibility": 0,
            "lifecycle": 2,
            "access_list": []
        }"#;
        let pre: StorageFileInfoV2 = serde_json::from_str(pre_v3_3).unwrap();
        assert!(pre.lifecycle.is_abandoned());
        assert_eq!(
            pre.abandoned_at_height, None,
            "pre-v3.3 chains omit the field; SNIP must default to None"
        );

        // Post-v3.3 chain: explicit value.
        let post_v3_3 = r#"{
            "merkle_root": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "owner": "ownerB58",
            "plaintext_size_bytes": 0,
            "stored_size_bytes": 0,
            "chunk_count": 1,
            "fee_pool": 0,
            "created_at": 100,
            "activated_at_height": null,
            "abandoned_at_height": 175,
            "assignment_height": 100,
            "visibility": 0,
            "lifecycle": 2,
            "access_list": []
        }"#;
        let post: StorageFileInfoV2 = serde_json::from_str(post_v3_3).unwrap();
        assert!(post.lifecycle.is_abandoned());
        assert_eq!(post.abandoned_at_height, Some(175));
    }

    #[test]
    fn block_height_info_round_trip() {
        let json = r#"{"height": 12345, "finality": "finalized"}"#;
        let info: BlockHeightInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.height, 12345);
        assert_eq!(info.finality, "finalized");

        // "latest" finality also valid.
        let json = r#"{"height": 12347, "finality": "latest"}"#;
        let info: BlockHeightInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.finality, "latest");
    }

    #[test]
    fn tx_status_v2_all_variants_round_trip() {
        // Wire shape (chain-confirmed): internally-tagged on `kind`,
        // snake_case variant names.
        let cases = [
            (r#"{"kind": "unknown"}"#, TxStatusV2::Unknown),
            (r#"{"kind": "pending"}"#, TxStatusV2::Pending),
            (
                r#"{"kind": "included", "block_height": 100}"#,
                TxStatusV2::Included { block_height: 100 },
            ),
            (
                r#"{"kind": "finalized", "block_height": 200}"#,
                TxStatusV2::Finalized { block_height: 200 },
            ),
            (
                r#"{"kind": "failed", "block_height": null, "reason": "low-order x25519 public key rejected"}"#,
                TxStatusV2::Failed {
                    block_height: None,
                    reason: "low-order x25519 public key rejected".into(),
                },
            ),
            (
                r#"{"kind": "failed", "block_height": 300, "reason": "insufficient balance"}"#,
                TxStatusV2::Failed {
                    block_height: Some(300),
                    reason: "insufficient balance".into(),
                },
            ),
            (r#"{"kind": "dropped"}"#, TxStatusV2::Dropped),
        ];
        for (json, expected) in cases {
            let actual: TxStatusV2 = serde_json::from_str(json).unwrap();
            assert_eq!(actual, expected, "mismatch on `{json}`");
            // Round-trip back to JSON and re-parse: must match original variant.
            let reserialized = serde_json::to_string(&actual).unwrap();
            let reparsed: TxStatusV2 = serde_json::from_str(&reserialized).unwrap();
            assert_eq!(
                reparsed, expected,
                "re-parse round-trip mismatch on `{json}`"
            );
        }
    }

    /// Pin the *exact* tag key. If anyone "fixes" the discriminant back
    /// to `"status"` later, this test fires loudly instead of letting a
    /// silent on-chain regression slip through. Reads each variant via
    /// the canonical wire shape and asserts the deserialized value is
    /// the *correct* enum case (not the default Unknown via fallback).
    #[test]
    fn tx_status_v2_tag_key_is_kind_not_status() {
        // The chain emits `kind`. If we keyed on `status` here, every
        // real chain response would land as a parse error.
        let json_kind = r#"{"kind": "finalized", "block_height": 7}"#;
        let parsed: TxStatusV2 = serde_json::from_str(json_kind).unwrap();
        assert!(matches!(parsed, TxStatusV2::Finalized { block_height: 7 }));

        // The legacy/wrong key MUST fail to deserialize. If it ever
        // succeeds again, this test catches the silent regression.
        let json_status = r#"{"status": "finalized", "block_height": 7}"#;
        let bad: Result<TxStatusV2, _> = serde_json::from_str(json_status);
        assert!(
            bad.is_err(),
            "deserializing on the legacy `status` key must fail; got {bad:?}"
        );
    }

    /// Per-variant contract assertions — exhaustively cover the six
    /// kinds the chain emits, each with its expected payload shape.
    /// Any one of these failing means the chain's response shape and
    /// SNIP's expectation have drifted.
    #[test]
    fn tx_status_v2_per_variant_contract() {
        // pending — tag-only.
        match serde_json::from_str::<TxStatusV2>(r#"{"kind":"pending"}"#).unwrap() {
            TxStatusV2::Pending => (),
            other => panic!("expected Pending, got {other:?}"),
        }
        // included — block_height required.
        match serde_json::from_str::<TxStatusV2>(r#"{"kind":"included","block_height":42}"#)
            .unwrap()
        {
            TxStatusV2::Included { block_height } => assert_eq!(block_height, 42),
            other => panic!("expected Included, got {other:?}"),
        }
        // finalized — block_height required.
        match serde_json::from_str::<TxStatusV2>(r#"{"kind":"finalized","block_height":99}"#)
            .unwrap()
        {
            TxStatusV2::Finalized { block_height } => assert_eq!(block_height, 99),
            other => panic!("expected Finalized, got {other:?}"),
        }
        // failed — block_height nullable, reason required.
        match serde_json::from_str::<TxStatusV2>(
            r#"{"kind":"failed","block_height":null,"reason":"x"}"#,
        )
        .unwrap()
        {
            TxStatusV2::Failed {
                block_height,
                reason,
            } => {
                assert!(block_height.is_none());
                assert_eq!(reason, "x");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // dropped — tag-only.
        match serde_json::from_str::<TxStatusV2>(r#"{"kind":"dropped"}"#).unwrap() {
            TxStatusV2::Dropped => (),
            other => panic!("expected Dropped, got {other:?}"),
        }
        // unknown — tag-only.
        match serde_json::from_str::<TxStatusV2>(r#"{"kind":"unknown"}"#).unwrap() {
            TxStatusV2::Unknown => (),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn assignment_coverage_v2_round_trip() {
        let json = r#"{
            "chunk_count": 10,
            "covered_count": 7,
            "can_activate_now": false,
            "missing_total": 3,
            "missing_offset": 0,
            "missing_indices": [3, 5, 9],
            "per_archive": [
                {
                    "archive": "archA",
                    "assigned_count": 5,
                    "attested_count": 4,
                    "currently_active": true
                },
                {
                    "archive": "archB",
                    "assigned_count": 5,
                    "attested_count": 3,
                    "currently_active": true
                }
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.chunk_count, 10);
        assert_eq!(cov.covered_count, 7);
        assert!(!cov.can_activate_now);
        assert_eq!(cov.missing_total, 3);
        assert_eq!(cov.missing_indices, vec![3, 5, 9]);
        assert_eq!(cov.per_archive.len(), 2);
        assert_eq!(cov.per_archive[0].assigned_count, Some(5));
        assert_eq!(cov.per_archive[1].attested_count, 3);
    }

    /// Chain plan v3.2 caps per-archive assignment work at
    /// the chain's internal per-archive count cap (currently 16,384). For files above that
    /// cap, every entry's `assigned_count` arrives as JSON `null` and
    /// the client must recompute locally. Pin that the `null` shape
    /// deserializes to `None` rather than tripping a parse error.
    #[test]
    fn assignment_coverage_v2_archive_assigned_count_null_for_large_files() {
        let json = r#"{
            "chunk_count": 1048576,
            "covered_count": 524288,
            "can_activate_now": false,
            "missing_total": 524288,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [
                {
                    "archive": "archA",
                    "assigned_count": null,
                    "attested_count": 200000,
                    "currently_active": true
                },
                {
                    "archive": "archB",
                    "assigned_count": null,
                    "attested_count": 324288,
                    "currently_active": true
                }
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.chunk_count, 1_048_576);
        assert_eq!(cov.per_archive.len(), 2);
        assert!(cov.per_archive[0].assigned_count.is_none());
        assert!(cov.per_archive[1].assigned_count.is_none());
        // Coverage fields stay populated regardless of the cap.
        assert_eq!(cov.covered_count, 524_288);
        assert_eq!(cov.missing_total, 524_288);
    }

    #[test]
    fn assignment_coverage_v2_can_activate_now_true_path() {
        // covered_count == chunk_count; missing list empty.
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": []
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert!(cov.can_activate_now);
        assert_eq!(cov.missing_total, 0);
    }

    // ── #34: AssignmentCoverageV2 post-#62 epoch fields ─────────────────

    /// Legacy pre-#62 chain: 7-field JSON deserializes cleanly; the
    /// four new fields land at their `#[serde(default)]` values;
    /// `resolved_latest_epoch()` returns `None` disambiguating this
    /// from a post-#62 response.
    #[test]
    fn assignment_coverage_v2_legacy_pre_62_deserializes() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 2,
            "can_activate_now": false,
            "missing_total": 2,
            "missing_offset": 0,
            "missing_indices": [2, 3],
            "per_archive": []
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert!(cov.assignment_epochs.is_empty());
        assert_eq!(cov.latest_assignment_epoch, 0);
        assert!(!cov.reassignment_needed);
        assert!(cov.per_epoch.is_empty());
        assert_eq!(cov.resolved_latest_epoch(), None);
    }

    /// Post-#62 single-epoch shape: all four fields present with
    /// `assignment_epochs = [500]`, `latest_assignment_epoch = 500`,
    /// one `per_epoch` entry marked epoch-0.
    #[test]
    fn assignment_coverage_v2_current_single_epoch_deserializes() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [
                {"archive": "archA", "assigned_count": 4, "attested_count": 4, "currently_active": true}
            ],
            "assignment_epochs": [500],
            "latest_assignment_epoch": 500,
            "reassignment_needed": false,
            "per_epoch": [
                {"epoch_height": 500, "is_epoch_zero": true, "covered_count": 4, "per_archive": []}
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.resolved_latest_epoch(), Some(500));
        assert_eq!(cov.per_epoch.len(), 1);
        assert!(cov.per_epoch[0].is_epoch_zero);
    }

    /// Multiple ascending epochs; `per_epoch.len() ==
    /// assignment_epochs.len()` invariant preserved.
    #[test]
    fn assignment_coverage_v2_multiple_epochs_ascending() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 3,
            "can_activate_now": false,
            "missing_total": 1,
            "missing_offset": 0,
            "missing_indices": [3],
            "per_archive": [],
            "assignment_epochs": [100, 200, 300],
            "latest_assignment_epoch": 300,
            "reassignment_needed": true,
            "per_epoch": [
                {"epoch_height": 100, "is_epoch_zero": true,  "covered_count": 2, "per_archive": []},
                {"epoch_height": 200, "is_epoch_zero": false, "covered_count": 2, "per_archive": []},
                {"epoch_height": 300, "is_epoch_zero": false, "covered_count": 1, "per_archive": []}
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.assignment_epochs.len(), cov.per_epoch.len());
        for (i, epoch) in cov.per_epoch.iter().enumerate() {
            assert_eq!(epoch.epoch_height, cov.assignment_epochs[i]);
        }
        assert_eq!(cov.latest_assignment_epoch, 300);
        assert!(cov.per_epoch[0].is_epoch_zero);
        assert!(!cov.per_epoch[1].is_epoch_zero);
    }

    /// `reassignment_needed: true` must decode to true — verify the
    /// `#[serde(default)]` doesn't mask a real value.
    #[test]
    fn assignment_coverage_v2_reassignment_required_true() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [],
            "assignment_epochs": [100, 500],
            "latest_assignment_epoch": 500,
            "reassignment_needed": true,
            "per_epoch": []
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert!(cov.reassignment_needed);
        // can_activate_now=true and reassignment_needed=true are
        // both valid simultaneously per upstream aggregate semantics.
        assert!(cov.can_activate_now);
    }

    /// Both `can_activate_now` and `reassignment_needed` true is a
    /// valid post-#62 state — activation is gated on aggregate
    /// coverage; reassignment is a separate flag.
    #[test]
    fn assignment_coverage_v2_reassignment_and_activate_now_both_true() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [],
            "assignment_epochs": [100, 500],
            "latest_assignment_epoch": 500,
            "reassignment_needed": true,
            "per_epoch": [
                {"epoch_height": 100, "is_epoch_zero": true,  "covered_count": 3, "per_archive": []},
                {"epoch_height": 500, "is_epoch_zero": false, "covered_count": 1, "per_archive": []}
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert!(cov.can_activate_now);
        assert!(cov.reassignment_needed);
    }

    /// Edge case: `assignment_epochs` present (non-empty) but
    /// `per_epoch` empty. This is a malformed partial-metadata state
    /// upstream should never emit; SNIP does not enforce the invariant
    /// at deserialize (documented limitation of the `#[serde(default)]`
    /// representation). `resolved_latest_epoch()` still returns
    /// `Some(...)` because `assignment_epochs` is non-empty.
    #[test]
    fn assignment_coverage_v2_empty_per_epoch_present_epochs() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 0,
            "can_activate_now": false,
            "missing_total": 4,
            "missing_offset": 0,
            "missing_indices": [0, 1, 2, 3],
            "per_archive": [],
            "assignment_epochs": [100],
            "latest_assignment_epoch": 100,
            "reassignment_needed": false,
            "per_epoch": []
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.resolved_latest_epoch(), Some(100));
    }

    /// Unknown future top-level field is silently ignored (no
    /// `#[serde(deny_unknown_fields)]` anywhere).
    #[test]
    fn assignment_coverage_v2_unknown_future_top_level_field_ignored() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [],
            "future_field": {"nested": [1, 2, 3]}
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert!(cov.can_activate_now);
    }

    /// Unknown future field inside a `per_epoch` entry is also
    /// silently ignored.
    #[test]
    fn assignment_coverage_v2_unknown_future_field_on_per_epoch_ignored() {
        let json = r#"{
            "chunk_count": 4,
            "covered_count": 4,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [],
            "assignment_epochs": [100],
            "latest_assignment_epoch": 100,
            "reassignment_needed": false,
            "per_epoch": [
                {"epoch_height": 100, "is_epoch_zero": true, "covered_count": 4, "per_archive": [], "future_field_on_epoch": true}
            ]
        }"#;
        let cov: AssignmentCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(cov.per_epoch.len(), 1);
    }

    /// Explicit round-trip test for the new inner type.
    #[test]
    fn assignment_epoch_coverage_v2_full_round_trip() {
        let json = r#"{
            "epoch_height": 42,
            "is_epoch_zero": false,
            "covered_count": 7,
            "per_archive": [
                {"archive": "archB", "assigned_count": 3, "attested_count": 2, "currently_active": true}
            ]
        }"#;
        let e: AssignmentEpochCoverageV2 = serde_json::from_str(json).unwrap();
        assert_eq!(e.epoch_height, 42);
        assert!(!e.is_epoch_zero);
        assert_eq!(e.covered_count, 7);
        assert_eq!(e.per_archive.len(), 1);
        // Serialize back and re-parse.
        let re = serde_json::to_string(&e).unwrap();
        let e2: AssignmentEpochCoverageV2 = serde_json::from_str(&re).unwrap();
        assert_eq!(e, e2);
    }

    #[test]
    fn resolved_latest_epoch_none_when_epochs_empty() {
        let cov = AssignmentCoverageV2 {
            chunk_count: 1,
            covered_count: 0,
            can_activate_now: false,
            missing_total: 1,
            missing_offset: 0,
            missing_indices: vec![0],
            per_archive: vec![],
            assignment_epochs: vec![],
            latest_assignment_epoch: 0,
            reassignment_needed: false,
            per_epoch: vec![],
        };
        assert_eq!(cov.resolved_latest_epoch(), None);
    }

    #[test]
    fn resolved_latest_epoch_some_when_epochs_populated() {
        let cov = AssignmentCoverageV2 {
            chunk_count: 1,
            covered_count: 1,
            can_activate_now: true,
            missing_total: 0,
            missing_offset: 0,
            missing_indices: vec![],
            per_archive: vec![],
            assignment_epochs: vec![42],
            latest_assignment_epoch: 42,
            reassignment_needed: false,
            per_epoch: vec![AssignmentEpochCoverageV2 {
                epoch_height: 42,
                is_epoch_zero: true,
                covered_count: 1,
                per_archive: vec![],
            }],
        };
        assert_eq!(cov.resolved_latest_epoch(), Some(42));
    }

    // ── chain_getChainParams wire-shape contract ────────────────────────
    //
    // These tests pin the wire shape SNIP commits to deserialize. They
    // intentionally use the EXACT field set the chain currently emits
    // (including the consensus / fee / metadata-cap fields SNIP doesn't
    // model, like `max_block_bytes`, `storage_fee_per_byte`, etc.) so
    // a future chain rev that drops or renames a field is caught HERE,
    // not at runtime when an operator's node fails to start.

    /// Full chain wire shape with every field present and numeric.
    /// `v2_enabled_from_height = 12_345` means V2 is on for finalized
    /// heights at and after block 12,345. Extra chain fields SNIP
    /// doesn't model (max_block_bytes, storage_fee_per_byte, etc.)
    /// must deserialize without error — serde silently drops unknown
    /// fields with the default config we use.
    #[test]
    fn chain_params_full_chain_shape_round_trips() {
        let json = r#"{
            "chain_id": 31337,
            "block_time_ms": 2000,
            "max_block_bytes": 8388608,
            "max_txs_per_block": 1024,
            "min_fee": 1000,
            "finality_depth": 3,
            "storage_fee_per_byte": 1,
            "max_metadata_bytes": 65536,
            "max_access_list_bytes": 16384,
            "activation_grace_blocks": 50,
            "abandonment_fee_percent": 10,
            "max_chunk_count_per_file": 1048576,
            "max_chunk_indices_per_tx": 65536,
            "assignment_replication_factor": 3,
            "v2_enabled_from_height": 12345
        }"#;
        let cp: ChainParamsInfo = serde_json::from_str(json).unwrap();
        assert_eq!(cp.chain_id, 31337);
        assert_eq!(cp.block_time_ms, 2000);
        assert_eq!(cp.finality_depth, 3);
        assert_eq!(cp.min_fee, 1000);
        assert_eq!(cp.assignment_replication_factor, 3);
        assert_eq!(cp.max_chunk_indices_per_tx, 65536);
        assert_eq!(cp.max_chunk_count_per_file, 1_048_576);
        assert_eq!(cp.activation_grace_blocks, 50);
        assert_eq!(cp.v2_enabled_from_height, Some(12345));
    }

    /// `v2_enabled_from_height: null` (chain hasn't activated V2 yet,
    /// or never will on this chain) MUST deserialize to `None` — not
    /// a parse error and not a silent default to 0. This is the
    /// load-bearing case: SNIP's V2 gate refuses tx submission when
    /// the field is `None`.
    #[test]
    fn chain_params_v2_enabled_from_height_null_is_none() {
        let json = r#"{
            "chain_id": 31337,
            "block_time_ms": 2000,
            "max_block_bytes": 8388608,
            "max_txs_per_block": 1024,
            "min_fee": 1000,
            "finality_depth": 3,
            "storage_fee_per_byte": 1,
            "max_metadata_bytes": 65536,
            "max_access_list_bytes": 16384,
            "activation_grace_blocks": 50,
            "abandonment_fee_percent": 10,
            "max_chunk_count_per_file": 1048576,
            "max_chunk_indices_per_tx": 65536,
            "assignment_replication_factor": 3,
            "v2_enabled_from_height": null
        }"#;
        let cp: ChainParamsInfo = serde_json::from_str(json).unwrap();
        assert_eq!(
            cp.v2_enabled_from_height, None,
            "JSON null MUST decode to None, not Some(0); the V2 gate \
             distinguishes 'no activation height set' from 'enabled at genesis'"
        );
    }

    /// Field omitted entirely (older chain version, or partial
    /// rollout). `#[serde(default)]` must leave it as `None`.
    #[test]
    fn chain_params_v2_enabled_from_height_missing_is_none() {
        let json = r#"{
            "chain_id": 31337,
            "block_time_ms": 2000,
            "max_block_bytes": 8388608,
            "max_txs_per_block": 1024,
            "min_fee": 1000,
            "finality_depth": 3,
            "storage_fee_per_byte": 1,
            "max_metadata_bytes": 65536,
            "max_access_list_bytes": 16384,
            "activation_grace_blocks": 50,
            "abandonment_fee_percent": 10,
            "max_chunk_count_per_file": 1048576,
            "max_chunk_indices_per_tx": 65536,
            "assignment_replication_factor": 3
        }"#;
        let cp: ChainParamsInfo = serde_json::from_str(json).unwrap();
        assert_eq!(cp.v2_enabled_from_height, None);
    }

    /// `v2_enabled_from_height: 0` is NOT `None` — it's a real,
    /// distinct semantic ("V2 enabled from genesis"). Pin that
    /// distinction so a future "let's just default null to 0"
    /// regression fires here.
    #[test]
    fn chain_params_v2_enabled_from_height_zero_is_some_zero() {
        let json = r#"{
            "chain_id": 31337,
            "block_time_ms": 2000,
            "max_block_bytes": 8388608,
            "max_txs_per_block": 1024,
            "min_fee": 1000,
            "finality_depth": 3,
            "storage_fee_per_byte": 1,
            "max_metadata_bytes": 65536,
            "max_access_list_bytes": 16384,
            "activation_grace_blocks": 50,
            "abandonment_fee_percent": 10,
            "max_chunk_count_per_file": 1048576,
            "max_chunk_indices_per_tx": 65536,
            "assignment_replication_factor": 3,
            "v2_enabled_from_height": 0
        }"#;
        let cp: ChainParamsInfo = serde_json::from_str(json).unwrap();
        assert_eq!(cp.v2_enabled_from_height, Some(0));
        assert_ne!(cp.v2_enabled_from_height, None);
    }

    // ── #32: eligibility contract ────────────────────────────────────────

    fn record(role: &str, status: &str) -> NodeRecordInfo {
        NodeRecordInfo {
            address: format!("addr_{role}_{status}"),
            role: role.into(),
            staked_balance: 1_000_000_000,
            status: status.into(),
            registered_at: 1,
        }
    }

    #[test]
    fn is_active_archive_true_only_for_archive_and_active() {
        let combos = [
            // Only this combination is eligible.
            ("ArchiveNode", "Active", true),
            // ArchiveNode with a non-Active status: ineligible.
            ("ArchiveNode", "Slashed", false),
            ("ArchiveNode", "Unbonding", false),
            ("ArchiveNode", "Withdrawn", false),
            // Unknown-future status is ineligible.
            ("ArchiveNode", "Frozen", false),
            // Non-ArchiveNode roles are ineligible even when Active.
            ("Validator", "Active", false),
            ("Validator", "Slashed", false),
            ("BogusRole", "Active", false),
            // Blank strings never satisfy the contract.
            ("", "Active", false),
            ("ArchiveNode", "", false),
        ];
        for (role, status, expected) in combos {
            let r = record(role, status);
            assert_eq!(
                r.is_active_archive(),
                expected,
                "is_active_archive for ({role:?}, {status:?})"
            );
        }
    }

    #[test]
    fn known_status_maps_the_four_known_variants() {
        assert_eq!(
            record("ArchiveNode", "Active").known_status(),
            Some(KnownNodeStatus::Active)
        );
        assert_eq!(
            record("ArchiveNode", "Slashed").known_status(),
            Some(KnownNodeStatus::Slashed)
        );
        assert_eq!(
            record("ArchiveNode", "Unbonding").known_status(),
            Some(KnownNodeStatus::Unbonding)
        );
        assert_eq!(
            record("ArchiveNode", "Withdrawn").known_status(),
            Some(KnownNodeStatus::Withdrawn)
        );
        // Unknown-future status.
        assert_eq!(record("ArchiveNode", "Frozen").known_status(), None);
    }

    #[test]
    fn node_record_info_deserializes_unknown_status_as_string() {
        // Chain adds a future variant SNIP doesn't know yet — SNIP must
        // still deserialize the record without error, expose the raw
        // string via `.status`, and treat it as ineligible.
        let json = r#"{
            "address": "SomeAddr",
            "role": "ArchiveNode",
            "staked_balance": 1000000000,
            "status": "Frozen",
            "registered_at": 42
        }"#;
        let info: NodeRecordInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.status, "Frozen");
        assert_eq!(info.known_status(), None);
        assert!(!info.is_active_archive());
    }

    #[test]
    fn filter_active_archives_narrows_mixed_snapshot() {
        // Every possible (role, status) combination from the eligibility
        // test above; exactly one — ArchiveNode/Active — should survive.
        let input = vec![
            record("ArchiveNode", "Active"),
            record("ArchiveNode", "Slashed"),
            record("ArchiveNode", "Unbonding"),
            record("ArchiveNode", "Withdrawn"),
            record("ArchiveNode", "Frozen"),
            record("Validator", "Active"),
            record("Validator", "Slashed"),
            record("BogusRole", "Active"),
            record("", "Active"),
            record("ArchiveNode", ""),
        ];
        let out = filter_active_archives(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "ArchiveNode");
        assert_eq!(out[0].status, "Active");
    }
}
