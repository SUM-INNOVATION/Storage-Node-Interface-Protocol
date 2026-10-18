//! Owner-triggered `ReassignChunksV2` for a file this operator owns.
//!
//! The chain decides when it will *accept* a reassignment: its
//! `storage_getAssignmentCoverageV2` response sets `reassignment_needed`
//! once any archive in the latest epoch's assignment snapshot has left the
//! active set, and the executor accepts `ReassignChunksV2` only from the
//! file's owner and only in that state. That rule is snapshot-wide: it fires
//! for every file whose latest snapshot included the departed archive, whether
//! or not that archive was assigned any chunk of the file.
//!
//! This tool is more conservative than the chain (the N2 policy): it submits
//! only when an archive that has left holds at least one chunk of this file
//! under the latest epoch. A departure that does not touch the file costs the
//! owner nothing.
//!
//! This module adds no signing authority. The flow is:
//!
//! 1. Read the file row and refuse unless its owner is this key's address.
//! 2. Read the chain's replication factor.
//! 3. Read coverage inside a latest-height bracket (see *One chain state*
//!    below) and refuse unless the chain reports `reassignment_needed`.
//! 4. **N2:** look for a *departed assignee*: an archive in the latest
//!    epoch's `per_archive` list with `currently_active == false` that is
//!    assigned at least one chunk. The chain's own `assigned_count` decides
//!    when the response carries it. Above the chain's 16,384-chunk cap the
//!    count is `null`, and the assignment is computed locally with the chain's
//!    scorer (`sum_store::assignment_v2`, a thin adapter over `sumchain-wire`)
//!    over that same list. No departed assignee: stop, nothing is built or
//!    signed.
//! 5. Ask the chain's no-key builder, `storage_buildReassignChunksV2`, for the
//!    unsigned transaction.
//! 6. Verify the returned bytes against the request: canonical encoding, chain
//!    id, sender, fee, the exact `ReassignChunksV2 { merkle_root }` payload,
//!    and that every echoed field and the signing hash agree with the decoded
//!    transaction. `ReassignChunksV2` carries no epoch field; the executor
//!    stamps the new epoch at the current height.
//! 7. With `dry_run`, stop here: nothing is signed or sent.
//! 8. Otherwise sign with the same owner key and signing path every other owner
//!    transaction uses ([`crate::tx_builder::sign_transaction_v2`]), submit, and
//!    wait for finality. A chain refusal is reported with its receipt reason.
//!
//! Every check fails closed: an RPC error, a missing row, a foreign owner, a
//! `reassignment_needed: false` response, no departed assignee, a coverage
//! response that cannot support the decision, or any mismatch in the built
//! bytes ends the call before anything is signed. The command runs only when
//! the owner invokes it; nothing here runs in the background.
//!
//! # One chain state
//!
//! The assignment inputs (the latest epoch's snapshot and the file's chunk
//! count) and the active-set inputs (`currently_active`, and the flag derived
//! from it) must describe the same chain state. Both come from **one**
//! coverage response: its latest `per_epoch.per_archive` list is exactly the
//! chain's archive snapshot at the latest epoch height, and the chain fills
//! `currently_active` for each entry in the same handler call. No second
//! snapshot RPC is mixed in.
//!
//! The coverage RPC takes no height and reports none, and it reads the node's
//! latest committed rows without a database snapshot. So the read is
//! bracketed: the latest block height is read before and after, and the
//! response is used only if the two are equal and the response is internally
//! consistent (flag set ⇒ some archive in the latest snapshot is inactive).
//! Otherwise it is read again, up to [`SNAPSHOT_ATTEMPTS`] times in all, then
//! the command refuses.
//!
//! Residual: the node can write some state while executing a block before it
//! advances its reported head, so equal heights cannot exclude every
//! interleaving. The chain re-evaluates its own acceptance rule when it
//! executes the transaction, so a stale read can at worst cost one fee for a
//! reassignment the chain still accepts, or skip one the owner can re-run; it
//! cannot make the chain accept an invalid reassignment.
//!
//! Scope: this submits the chain transaction only. SNIP's push, attestation
//! and download paths still use the file's original assignment snapshot, so
//! a reassignment does not by itself restore coverage under the new epoch.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use sum_types::rpc_types::{
    AssignmentCoverageV2, ChainParamsInfo, StorageBuildResponse, StorageFileInfoV2,
};
use sumchain_wire::{
    Address, Hash,
    storage_metadata::{StorageMetadataOperationV2, StorageMetadataV2TxData},
    transaction::{TransactionV2, TxPayload},
};
use tracing::info;

use crate::rpc_client::L1RpcClient;
use crate::tx_wait::{TxStatusSource, TxWaitError, wait_for_finalized};

/// Bracketed coverage reads before the command gives up on one consistent
/// chain state. Blocks arrive every few seconds and one read takes
/// milliseconds, so a second attempt almost always lands inside one block.
pub const SNAPSHOT_ATTEMPTS: usize = 3;

/// Chain calls the reassignment flow needs. Production impl on
/// [`L1RpcClient`]; tests provide a scripted mock.
#[async_trait]
pub trait ReassignRpc: TxStatusSource + Send + Sync {
    /// `Ok(None)` — the chain has no V2 row for this root.
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>>;
    /// `Ok(None)` — the chain has no coverage row for this root.
    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<AssignmentCoverageV2>>;
    async fn chain_get_chain_params(&self) -> Result<ChainParamsInfo>;
    /// The latest included block height (not the finalized one).
    async fn chain_get_latest_block_height(&self) -> Result<u64>;
    async fn storage_build_reassign_chunks_v2(
        &self,
        from_base58: &str,
        merkle_root_hex: &str,
        fee: u128,
    ) -> Result<StorageBuildResponse>;
    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String>;
}

#[async_trait]
impl ReassignRpc for L1RpcClient {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>> {
        L1RpcClient::storage_get_file_info_v2(self, merkle_root_hex, None, None).await
    }

    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<AssignmentCoverageV2>> {
        // The flow reads the flags and the per-epoch archive lists, never the
        // missing-index list, so ask for an empty missing-index window.
        L1RpcClient::storage_get_assignment_coverage_v2(self, merkle_root_hex, None, Some(0)).await
    }

    async fn chain_get_chain_params(&self) -> Result<ChainParamsInfo> {
        L1RpcClient::chain_get_chain_params(self).await
    }

    async fn chain_get_latest_block_height(&self) -> Result<u64> {
        Ok(L1RpcClient::chain_get_latest_block_height(self)
            .await?
            .height)
    }

    async fn storage_build_reassign_chunks_v2(
        &self,
        from_base58: &str,
        merkle_root_hex: &str,
        fee: u128,
    ) -> Result<StorageBuildResponse> {
        L1RpcClient::storage_build_reassign_chunks_v2(self, from_base58, merkle_root_hex, fee).await
    }

    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
        L1RpcClient::send_raw_transaction(self, signed_tx_hex).await
    }
}

/// Inputs for one [`reassign_chunks_v2`] call.
#[derive(Debug, Clone)]
pub struct ReassignRequest {
    /// The file's 32-byte Merkle root.
    pub merkle_root: [u8; 32],
    /// Chain id the built transaction must carry.
    pub chain_id: u64,
    /// Fee to request from the builder; the built transaction must carry
    /// exactly this fee.
    pub fee: u128,
    /// Verify the built transaction, then stop without signing or sending.
    pub dry_run: bool,
    pub poll_interval: Duration,
    pub finality_timeout: Duration,
}

// ── N2: is there a departed assignee? ───────────────────────────────────────

/// How the N2 check established that a departed archive holds a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignedEvidence {
    /// The chain's own `assigned_count` for the latest epoch.
    ChainCount(u32),
    /// Computed locally because the chain elides the count above its cap:
    /// the first chunk index found assigned to the archive.
    LocalFirstChunk(u32),
}

/// An archive that has left the active set and holds at least one chunk of
/// the file under the latest epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepartedAssignee {
    /// L1 address in base58.
    pub archive: String,
    pub evidence: AssignedEvidence,
}

/// The N2 verdict for one coverage response with `reassignment_needed` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum N2Verdict {
    /// Submit: a departed archive holds a chunk.
    Departed(DepartedAssignee),
    /// Do not submit: archives in the latest snapshot have left (base58
    /// addresses listed), but none holds a chunk of this file.
    NoDepartedAssignee { departed: Vec<String> },
}

/// Why a coverage response cannot support the N2 decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum N2Error {
    /// The response contradicts itself, as a read that straddles a block can.
    /// Read again.
    Inconsistent(String),
    /// The response lacks what the decision needs. Refuse.
    Unusable(String),
}

/// Source of the per-chunk assignment for files whose counts the chain elides.
/// Production uses [`ChainScorer`]; tests can inject an oracle to exercise both
/// answers above the cap.
pub trait AssignmentOracle: Send + Sync {
    /// The first chunk index in `[0, chunk_count)` assigned to any of
    /// `targets` under `snapshot`, with that target; `None` if none is.
    fn first_assigned_chunk(
        &self,
        merkle_root: &[u8; 32],
        chunk_count: u32,
        snapshot: &[[u8; 20]],
        replication_factor: u32,
        targets: &[[u8; 20]],
    ) -> Option<(u32, [u8; 20])>;
}

/// The chain's deterministic V2 assignment, through
/// [`sum_store::assignment_v2::first_chunk_assigned_to_any`].
pub struct ChainScorer;

impl AssignmentOracle for ChainScorer {
    fn first_assigned_chunk(
        &self,
        merkle_root: &[u8; 32],
        chunk_count: u32,
        snapshot: &[[u8; 20]],
        replication_factor: u32,
        targets: &[[u8; 20]],
    ) -> Option<(u32, [u8; 20])> {
        sum_store::assignment_v2::first_chunk_assigned_to_any(
            merkle_root,
            chunk_count,
            snapshot,
            replication_factor,
            targets,
        )
    }
}

/// Apply N2 to a coverage response that has `reassignment_needed` set.
///
/// Uses only the latest `per_epoch` entry: its `per_archive` list is the
/// chain's snapshot at the latest epoch, and each entry's `currently_active`
/// comes from the same response. An inactive entry with `assigned_count:
/// Some(n > 0)` is a departed assignee on the chain's word. Inactive entries
/// with `assigned_count: None` (the chain elides counts above its cap) are
/// resolved with `oracle` over that same list.
pub fn evaluate_n2(
    coverage: &AssignmentCoverageV2,
    merkle_root: &[u8; 32],
    replication_factor: u32,
    oracle: &dyn AssignmentOracle,
) -> std::result::Result<N2Verdict, N2Error> {
    if replication_factor == 0 {
        return Err(N2Error::Unusable(
            "the chain reports assignment_replication_factor 0".into(),
        ));
    }
    let latest_epoch = coverage.resolved_latest_epoch().ok_or_else(|| {
        N2Error::Unusable(
            "the coverage response has no assignment epochs (pre-reassignment chain)".into(),
        )
    })?;
    if coverage.per_epoch.len() != coverage.assignment_epochs.len() {
        return Err(N2Error::Unusable(format!(
            "per_epoch has {} entries for {} assignment epochs",
            coverage.per_epoch.len(),
            coverage.assignment_epochs.len()
        )));
    }
    let latest = coverage
        .per_epoch
        .last()
        .ok_or_else(|| N2Error::Unusable("per_epoch is empty".into()))?;
    if latest.epoch_height != latest_epoch {
        return Err(N2Error::Unusable(format!(
            "the last per_epoch entry is for height {}, not the latest epoch {latest_epoch}",
            latest.epoch_height
        )));
    }

    let mut snapshot: Vec<[u8; 20]> = Vec::with_capacity(latest.per_archive.len());
    for entry in &latest.per_archive {
        let addr = Address::from_base58(&entry.archive).map_err(|e| {
            N2Error::Unusable(format!(
                "archive {:?} is not an L1 address: {e}",
                entry.archive
            ))
        })?;
        snapshot.push(*addr.as_bytes());
    }

    let departed: Vec<usize> = (0..latest.per_archive.len())
        .filter(|&i| !latest.per_archive[i].currently_active)
        .collect();
    if departed.is_empty() {
        return Err(N2Error::Inconsistent(
            "reassignment_needed is set, but every archive in the latest snapshot is active".into(),
        ));
    }

    // The chain's own counts first.
    for &i in &departed {
        let entry = &latest.per_archive[i];
        if let Some(n) = entry.assigned_count
            && n > 0
        {
            return Ok(N2Verdict::Departed(DepartedAssignee {
                archive: entry.archive.clone(),
                evidence: AssignedEvidence::ChainCount(n),
            }));
        }
    }

    // Then the local computation, for the entries the chain left uncounted.
    let uncounted: Vec<[u8; 20]> = departed
        .iter()
        .filter(|&&i| latest.per_archive[i].assigned_count.is_none())
        .map(|&i| snapshot[i])
        .collect();
    if !uncounted.is_empty()
        && let Some((chunk, who)) = oracle.first_assigned_chunk(
            merkle_root,
            coverage.chunk_count,
            &snapshot,
            replication_factor,
            &uncounted,
        )
    {
        return Ok(N2Verdict::Departed(DepartedAssignee {
            archive: Address::new(who).to_base58(),
            evidence: AssignedEvidence::LocalFirstChunk(chunk),
        }));
    }

    Ok(N2Verdict::NoDepartedAssignee {
        departed: departed
            .iter()
            .map(|&i| latest.per_archive[i].archive.clone())
            .collect(),
    })
}

// ── Outcomes ────────────────────────────────────────────────────────────────

/// Terminal outcome of [`reassign_chunks_v2`].
#[derive(Debug)]
pub enum ReassignOutcome {
    /// `ReassignChunksV2` finalized at `finalized_at_height`.
    Reassigned {
        tx_hash: String,
        finalized_at_height: u64,
        departed: DepartedAssignee,
    },
    /// `dry_run`: the built transaction verified; nothing was signed or sent.
    DryRun {
        nonce: u64,
        fee: u128,
        signing_hash: String,
        departed: DepartedAssignee,
        /// Latest height the coverage read was bracketed by.
        at_height: u64,
    },
    /// The chain has no V2 row (or no coverage row) for this root.
    NotRegistered,
    /// The file belongs to another address; this key never signs for it.
    NotOwner { owner: String },
    /// The chain does not report `reassignment_needed`; submitting would only
    /// burn a fee.
    NotNeeded {
        latest_assignment_epoch: Option<u64>,
    },
    /// N2: the chain would accept a reassignment, but no archive that has
    /// left holds a chunk of this file. Nothing was built or signed.
    NoDepartedAssignee {
        latest_assignment_epoch: u64,
        departed: Vec<String>,
        at_height: u64,
    },
    /// No coverage read inside [`SNAPSHOT_ATTEMPTS`] came from one consistent
    /// chain state. Nothing was built or signed.
    InconsistentSnapshot { attempts: usize, reason: String },
    /// The coverage response cannot support the N2 decision. Nothing was
    /// built or signed.
    UnusableCoverage { reason: String },
    /// The builder's output does not match the request; nothing was signed.
    BuildMismatch { reason: String },
    /// The chain executed and refused the transaction. `reason` is the
    /// chain's receipt reason, which carries its refusal code.
    Refused {
        tx_hash: String,
        block_height: Option<u64>,
        reason: String,
    },
    /// An RPC, signing, submission or finality-wait failure.
    Failed { source: anyhow::Error },
}

/// Verify a `storage_buildReassignChunksV2` response against the request
/// and return the decoded unsigned transaction. Fails closed on any
/// disagreement; the error names the first mismatch found.
pub fn verify_unsigned_reassign(
    resp: &StorageBuildResponse,
    expected_from: &Address,
    merkle_root: &[u8; 32],
    chain_id: u64,
    fee: u128,
) -> std::result::Result<TransactionV2, String> {
    let bytes = decode_hex_field("unsigned_tx", &resp.unsigned_tx)?;
    let tx = TransactionV2::from_bytes(&bytes)
        .map_err(|e| format!("unsigned_tx does not decode as one TransactionV2: {e}"))?;
    if tx.to_bytes() != bytes {
        return Err("unsigned_tx is not the canonical encoding of its transaction".into());
    }
    if tx.chain_id != chain_id {
        return Err(format!("chain_id is {}, expected {chain_id}", tx.chain_id));
    }
    if tx.from != *expected_from {
        return Err(format!(
            "sender is {}, expected {}",
            tx.from.to_base58(),
            expected_from.to_base58()
        ));
    }
    if tx.fee != fee {
        return Err(format!("fee is {}, expected {fee}", tx.fee));
    }
    let expected_payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::ReassignChunksV2 {
            merkle_root: Hash::new(*merkle_root),
        },
    });
    if tx.payload != expected_payload {
        return Err(format!(
            "payload is not ReassignChunksV2 for 0x{}",
            hex::encode(merkle_root)
        ));
    }
    if resp.from != tx.from.to_base58() {
        return Err(format!(
            "response `from` {} disagrees with the transaction sender",
            resp.from
        ));
    }
    if resp.nonce != tx.nonce {
        return Err(format!(
            "response nonce {} disagrees with the transaction nonce {}",
            resp.nonce, tx.nonce
        ));
    }
    if resp.fee != tx.fee {
        return Err(format!(
            "response fee {} disagrees with the transaction fee {}",
            resp.fee, tx.fee
        ));
    }
    if resp.chain_id != tx.chain_id {
        return Err(format!(
            "response chain_id {} disagrees with the transaction chain_id {}",
            resp.chain_id, tx.chain_id
        ));
    }
    let signing_hash = decode_hex_field("signing_hash", &resp.signing_hash)?;
    if signing_hash.as_slice() != tx.signing_hash().as_bytes() {
        return Err("response signing_hash is not the hash of unsigned_tx".into());
    }
    Ok(tx)
}

fn decode_hex_field(name: &str, value: &str) -> std::result::Result<Vec<u8>, String> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    hex::decode(digits).map_err(|e| format!("{name} is not hex: {e}"))
}

/// Run the owner-triggered reassignment for one file. See the module docs
/// for the flow and its fail-closed checks.
pub async fn reassign_chunks_v2<R: ReassignRpc + ?Sized>(
    rpc: &R,
    signing_key_seed: &[u8; 32],
    req: &ReassignRequest,
) -> ReassignOutcome {
    reassign_chunks_v2_with(rpc, signing_key_seed, req, &ChainScorer).await
}

/// [`reassign_chunks_v2`] with an explicit [`AssignmentOracle`]. Production
/// callers use [`reassign_chunks_v2`], which passes [`ChainScorer`].
pub async fn reassign_chunks_v2_with<R: ReassignRpc + ?Sized>(
    rpc: &R,
    signing_key_seed: &[u8; 32],
    req: &ReassignRequest,
    oracle: &dyn AssignmentOracle,
) -> ReassignOutcome {
    let own = Address::from_public_key(
        &SigningKey::from_bytes(signing_key_seed)
            .verifying_key()
            .to_bytes(),
    );
    let own_b58 = own.to_base58();
    let root_hex = format!("0x{}", hex::encode(req.merkle_root));

    let info = match rpc.storage_get_file_info_v2(&root_hex).await {
        Ok(Some(info)) => info,
        Ok(None) => return ReassignOutcome::NotRegistered,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    if info.owner != own_b58 {
        return ReassignOutcome::NotOwner { owner: info.owner };
    }

    let replication_factor = match rpc.chain_get_chain_params().await {
        Ok(p) => p.assignment_replication_factor,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };

    // One consistent chain state: bracketed coverage reads (module docs).
    let mut decided: Option<(N2Verdict, AssignmentCoverageV2, u64)> = None;
    let mut last_reason = String::new();
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = match rpc.chain_get_latest_block_height().await {
            Ok(h) => h,
            Err(e) => return ReassignOutcome::Failed { source: e },
        };
        let coverage = match rpc.storage_get_assignment_coverage_v2(&root_hex).await {
            Ok(Some(c)) => c,
            Ok(None) => return ReassignOutcome::NotRegistered,
            Err(e) => return ReassignOutcome::Failed { source: e },
        };
        // Not submitting is safe whatever state the read saw.
        if !coverage.reassignment_needed {
            return ReassignOutcome::NotNeeded {
                latest_assignment_epoch: coverage.resolved_latest_epoch(),
            };
        }
        let after = match rpc.chain_get_latest_block_height().await {
            Ok(h) => h,
            Err(e) => return ReassignOutcome::Failed { source: e },
        };
        if before != after {
            last_reason = format!(
                "a block was applied during the coverage read (latest height {before} -> {after})"
            );
            continue;
        }
        match evaluate_n2(&coverage, &req.merkle_root, replication_factor, oracle) {
            Ok(verdict) => {
                decided = Some((verdict, coverage, before));
                break;
            }
            Err(N2Error::Inconsistent(reason)) => last_reason = reason,
            Err(N2Error::Unusable(reason)) => return ReassignOutcome::UnusableCoverage { reason },
        }
    }
    let Some((verdict, coverage, at_height)) = decided else {
        return ReassignOutcome::InconsistentSnapshot {
            attempts: SNAPSHOT_ATTEMPTS,
            reason: last_reason,
        };
    };
    let departed = match verdict {
        N2Verdict::Departed(d) => d,
        N2Verdict::NoDepartedAssignee { departed } => {
            return ReassignOutcome::NoDepartedAssignee {
                latest_assignment_epoch: coverage.latest_assignment_epoch,
                departed,
                at_height,
            };
        }
    };
    info!(
        root = %root_hex,
        departed = %departed.archive,
        evidence = ?departed.evidence,
        at_height,
        "N2: a departed archive holds a chunk under the latest epoch"
    );

    let resp = match rpc
        .storage_build_reassign_chunks_v2(&own_b58, &root_hex, req.fee)
        .await
    {
        Ok(r) => r,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    let tx = match verify_unsigned_reassign(&resp, &own, &req.merkle_root, req.chain_id, req.fee) {
        Ok(tx) => tx,
        Err(reason) => return ReassignOutcome::BuildMismatch { reason },
    };

    if req.dry_run {
        return ReassignOutcome::DryRun {
            nonce: tx.nonce,
            fee: tx.fee,
            signing_hash: resp.signing_hash,
            departed,
            at_height,
        };
    }

    let signed_hex = match crate::tx_builder::sign_transaction_v2(signing_key_seed, tx) {
        Ok(h) => h,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    let tx_hash = match rpc.send_raw_transaction(&signed_hex).await {
        Ok(h) => h,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    match wait_for_finalized(rpc, &tx_hash, req.poll_interval, req.finality_timeout).await {
        Ok(height) => {
            info!(root = %root_hex, %tx_hash, finalized_at_height = height, "ReassignChunksV2 finalized");
            ReassignOutcome::Reassigned {
                tx_hash,
                finalized_at_height: height,
                departed,
            }
        }
        Err(TxWaitError::Failed {
            block_height,
            reason,
        }) => ReassignOutcome::Refused {
            tx_hash,
            block_height,
            reason,
        },
        Err(TxWaitError::Dropped) => ReassignOutcome::Failed {
            source: anyhow::anyhow!(
                "ReassignChunksV2 {tx_hash} dropped before inclusion; re-run the command"
            ),
        },
        Err(TxWaitError::Timeout { last_status }) => ReassignOutcome::Failed {
            source: anyhow::anyhow!(
                "ReassignChunksV2 {tx_hash}: finality timeout; last status {last_status:?}"
            ),
        },
        Err(TxWaitError::Rpc(e)) => ReassignOutcome::Failed { source: e },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use std::sync::Mutex;
    use sum_types::rpc_types::{LifecycleV2, TxStatusV2};
    use sumchain_wire::transaction::{SignedTransaction, TxInner};

    const SEED: [u8; 32] = [0x11; 32];
    const OTHER_SEED: [u8; 32] = [0x22; 32];
    const ROOT: [u8; 32] = [0x5a; 32];
    const CHAIN_ID: u64 = 1;
    const FEE: u128 = 1_000;
    const NONCE: u64 = 9;
    const R: u32 = 3;

    fn addr(seed: &[u8; 32]) -> Address {
        Address::from_public_key(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
    }

    /// Archive addresses used in coverage fixtures.
    fn archive(byte: u8) -> String {
        Address::new([byte; 20]).to_base58()
    }
    const STAYS: u8 = 0x31;
    const LEFT: u8 = 0x33;

    fn root_hex() -> String {
        format!("0x{}", hex::encode(ROOT))
    }

    fn file_info(owner: &Address) -> StorageFileInfoV2 {
        serde_json::from_value(serde_json::json!({
            "merkle_root": root_hex(),
            "owner": owner.to_base58(),
            "plaintext_size_bytes": 2048,
            "stored_size_bytes": 2048,
            "chunk_count": 2,
            "fee_pool": 5000,
            "created_at": 100,
            "activated_at_height": 120,
            "abandoned_at_height": null,
            "assignment_height": 100,
            "lifecycle": LifecycleV2::ACTIVE.0,
            "visibility": 0,
            "access_list": [],
        }))
        .expect("file info fixture")
    }

    fn entry(byte: u8, assigned: Option<u32>, active: bool) -> serde_json::Value {
        serde_json::json!({
            "archive": archive(byte),
            "assigned_count": assigned,
            "attested_count": 0,
            "currently_active": active,
        })
    }

    /// A coverage response with one epoch at height 100 whose snapshot is
    /// `latest`.
    fn coverage_with(
        needed: bool,
        chunk_count: u32,
        latest: Vec<serde_json::Value>,
    ) -> AssignmentCoverageV2 {
        serde_json::from_value(serde_json::json!({
            "chunk_count": chunk_count,
            "covered_count": chunk_count,
            "can_activate_now": false,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": latest.clone(),
            "assignment_epochs": [100],
            "latest_assignment_epoch": 100,
            "reassignment_needed": needed,
            "per_epoch": [{
                "epoch_height": 100,
                "is_epoch_zero": true,
                "covered_count": chunk_count,
                "per_archive": latest,
            }],
        }))
        .expect("coverage fixture")
    }

    /// Default: a departed archive the chain counts as holding one chunk.
    fn coverage(needed: bool) -> AssignmentCoverageV2 {
        coverage_with(
            needed,
            2,
            vec![entry(STAYS, Some(2), true), entry(LEFT, Some(1), false)],
        )
    }

    fn chain_params(r: u32) -> ChainParamsInfo {
        ChainParamsInfo {
            chain_id: CHAIN_ID,
            block_time_ms: 3000,
            finality_depth: 6,
            min_fee: 1_000,
            assignment_replication_factor: r,
            max_chunk_indices_per_tx: 65_536,
            max_chunk_count_per_file: 1_048_576,
            activation_grace_blocks: 50,
            v2_enabled_from_height: Some(0),
        }
    }

    /// What an honest chain builder returns for `from`/`root`.
    fn honest_build(
        from: &Address,
        root: [u8; 32],
        chain_id: u64,
        fee: u128,
    ) -> StorageBuildResponse {
        build_response(&TransactionV2 {
            chain_id,
            from: *from,
            fee,
            nonce: NONCE,
            payload: TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
                operation: StorageMetadataOperationV2::ReassignChunksV2 {
                    merkle_root: Hash::new(root),
                },
            }),
        })
    }

    fn build_response(tx: &TransactionV2) -> StorageBuildResponse {
        StorageBuildResponse {
            unsigned_tx: format!("0x{}", hex::encode(tx.to_bytes())),
            signing_hash: format!("0x{}", hex::encode(tx.signing_hash().as_bytes())),
            from: tx.from.to_base58(),
            nonce: tx.nonce,
            fee: tx.fee,
            chain_id: tx.chain_id,
        }
    }

    fn request(dry_run: bool) -> ReassignRequest {
        ReassignRequest {
            merkle_root: ROOT,
            chain_id: CHAIN_ID,
            fee: FEE,
            dry_run,
            poll_interval: Duration::from_millis(1),
            finality_timeout: Duration::from_millis(200),
        }
    }

    #[derive(Clone)]
    enum Reply<T> {
        Ok(T),
        Err(&'static str),
    }

    impl<T: Clone> Reply<T> {
        fn get(&self) -> Result<T> {
            match self {
                Reply::Ok(v) => Ok(v.clone()),
                Reply::Err(e) => Err(anyhow::anyhow!(*e)),
            }
        }
    }

    /// Replays a script one reply per call, repeating the last reply.
    struct Script<T> {
        replies: Vec<T>,
        calls: Mutex<usize>,
    }

    impl<T: Clone> Script<T> {
        fn new(replies: Vec<T>) -> Self {
            assert!(!replies.is_empty());
            Script {
                replies,
                calls: Mutex::new(0),
            }
        }
        fn next(&self) -> T {
            let mut n = self.calls.lock().unwrap();
            let r = self.replies[(*n).min(self.replies.len() - 1)].clone();
            *n += 1;
            r
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    struct MockRpc {
        info: Reply<Option<StorageFileInfoV2>>,
        params: Reply<ChainParamsInfo>,
        heights: Script<Reply<u64>>,
        coverage: Script<Reply<Option<AssignmentCoverageV2>>>,
        build: Reply<StorageBuildResponse>,
        send: Reply<String>,
        status: TxStatusV2,
        build_calls: Mutex<Vec<(String, String, u128)>>,
        sent: Mutex<Vec<String>>,
    }

    impl MockRpc {
        fn happy() -> Self {
            Self::with_coverage(vec![coverage(true)])
        }
        fn with_coverage(responses: Vec<AssignmentCoverageV2>) -> Self {
            let own = addr(&SEED);
            MockRpc {
                info: Reply::Ok(Some(file_info(&own))),
                params: Reply::Ok(chain_params(R)),
                heights: Script::new(vec![Reply::Ok(500)]),
                coverage: Script::new(responses.into_iter().map(|c| Reply::Ok(Some(c))).collect()),
                build: Reply::Ok(honest_build(&own, ROOT, CHAIN_ID, FEE)),
                send: Reply::Ok("0xfeed".into()),
                status: TxStatusV2::Finalized { block_height: 777 },
                build_calls: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            }
        }
        fn built(&self) -> usize {
            self.build_calls.lock().unwrap().len()
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
        /// The command decided not to submit: it never asked the builder for
        /// a transaction, so it never had bytes to sign, and sent nothing.
        fn assert_untouched(&self, case: &str) {
            assert_eq!(self.built(), 0, "{case}: the builder must not be called");
            assert!(self.sent().is_empty(), "{case}: nothing may be sent");
        }
    }

    #[async_trait]
    impl TxStatusSource for MockRpc {
        async fn get_transaction_status(&self, _tx_hash: &str) -> Result<TxStatusV2> {
            Ok(self.status.clone())
        }
    }

    #[async_trait]
    impl ReassignRpc for MockRpc {
        async fn storage_get_file_info_v2(&self, _: &str) -> Result<Option<StorageFileInfoV2>> {
            self.info.get()
        }
        async fn storage_get_assignment_coverage_v2(
            &self,
            _: &str,
        ) -> Result<Option<AssignmentCoverageV2>> {
            self.coverage.next().get()
        }
        async fn chain_get_chain_params(&self) -> Result<ChainParamsInfo> {
            self.params.get()
        }
        async fn chain_get_latest_block_height(&self) -> Result<u64> {
            self.heights.next().get()
        }
        async fn storage_build_reassign_chunks_v2(
            &self,
            from_base58: &str,
            merkle_root_hex: &str,
            fee: u128,
        ) -> Result<StorageBuildResponse> {
            self.build_calls.lock().unwrap().push((
                from_base58.to_string(),
                merkle_root_hex.to_string(),
                fee,
            ));
            self.build.get()
        }
        async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
            self.sent.lock().unwrap().push(signed_tx_hex.to_string());
            self.send.get()
        }
    }

    /// An oracle with a fixed answer that records what it was asked.
    struct FixedOracle {
        answer: Option<(u32, [u8; 20])>,
        asked: Mutex<Vec<(u32, Vec<[u8; 20]>, u32, Vec<[u8; 20]>)>>,
    }

    impl FixedOracle {
        fn new(answer: Option<(u32, [u8; 20])>) -> Self {
            FixedOracle {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl AssignmentOracle for FixedOracle {
        fn first_assigned_chunk(
            &self,
            _merkle_root: &[u8; 32],
            chunk_count: u32,
            snapshot: &[[u8; 20]],
            replication_factor: u32,
            targets: &[[u8; 20]],
        ) -> Option<(u32, [u8; 20])> {
            self.asked.lock().unwrap().push((
                chunk_count,
                snapshot.to_vec(),
                replication_factor,
                targets.to_vec(),
            ));
            self.answer
        }
    }

    #[tokio::test]
    async fn happy_path_signs_the_built_tx_with_the_owner_key_and_waits_for_finality() {
        let rpc = MockRpc::happy();
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        match out {
            ReassignOutcome::Reassigned {
                tx_hash,
                finalized_at_height,
                departed,
            } => {
                assert_eq!(tx_hash, "0xfeed");
                assert_eq!(finalized_at_height, 777);
                assert_eq!(
                    departed,
                    DepartedAssignee {
                        archive: archive(LEFT),
                        evidence: AssignedEvidence::ChainCount(1),
                    }
                );
            }
            other => panic!("expected Reassigned, got {other:?}"),
        }

        // The builder was asked for this owner, this root and this fee.
        let calls = rpc.build_calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(addr(&SEED).to_base58(), root_hex(), FEE)]);

        // Exactly one transaction went out: the built one, signed by SEED.
        let sent = rpc.sent();
        assert_eq!(sent.len(), 1);
        let signed = SignedTransaction::from_hex(&sent[0]).expect("signed tx decodes");
        let tx = match &signed.inner {
            TxInner::V2(tx) => tx,
            other => panic!("expected a V2 signed transaction, got {other:?}"),
        };
        let (sig, pubkey) = (signed.signature, signed.public_key);
        let expected = honest_build(&addr(&SEED), ROOT, CHAIN_ID, FEE);
        assert_eq!(
            format!("0x{}", hex::encode(tx.to_bytes())),
            expected.unsigned_tx,
            "the signed transaction is the built one, unchanged"
        );
        assert_eq!(
            pubkey,
            SigningKey::from_bytes(&SEED).verifying_key().to_bytes()
        );
        VerifyingKey::from_bytes(&pubkey)
            .unwrap()
            .verify(tx.signing_hash().as_bytes(), &Signature::from_bytes(&sig))
            .expect("signature verifies over the signing hash");
    }

    #[tokio::test]
    async fn not_needed_is_refused_before_building_or_signing() {
        let rpc = MockRpc::with_coverage(vec![coverage(false)]);
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(
                out,
                ReassignOutcome::NotNeeded {
                    latest_assignment_epoch: Some(100)
                }
            ),
            "got {out:?}"
        );
        rpc.assert_untouched("flag false");
    }

    /// N2: the chain would accept a reassignment because an archive left the
    /// latest snapshot, but the chain's own counts say it holds nothing of
    /// this file.
    #[tokio::test]
    async fn flag_set_but_every_departed_archive_holds_nothing_is_not_submitted() {
        let cov = coverage_with(
            true,
            2,
            vec![
                entry(STAYS, Some(2), true),
                entry(LEFT, Some(0), false),
                entry(0x34, Some(0), false),
            ],
        );
        for dry_run in [false, true] {
            let rpc = MockRpc::with_coverage(vec![cov.clone()]);
            let out = reassign_chunks_v2(&rpc, &SEED, &request(dry_run)).await;
            match out {
                ReassignOutcome::NoDepartedAssignee {
                    latest_assignment_epoch,
                    departed,
                    at_height,
                } => {
                    assert_eq!(latest_assignment_epoch, 100);
                    assert_eq!(departed, vec![archive(LEFT), archive(0x34)]);
                    assert_eq!(at_height, 500);
                }
                other => panic!("expected NoDepartedAssignee, got {other:?}"),
            }
            rpc.assert_untouched("no departed assignee");
        }
    }

    /// Above the chain's cap the counts are `null`: the real scorer, over the
    /// response's own snapshot, finds the departed archive's chunks.
    #[tokio::test]
    async fn above_the_cap_a_departed_assignee_is_found_by_the_chain_scorer() {
        let snapshot: Vec<u8> = vec![0x31, 0x32, 0x33, 0x34, 0x35];
        let latest: Vec<serde_json::Value> = snapshot
            .iter()
            .map(|&b| entry(b, None, b != LEFT))
            .collect();
        let cov = coverage_with(true, 16_385, latest);
        let rpc = MockRpc::with_coverage(vec![cov]);
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        let departed = match out {
            ReassignOutcome::DryRun { departed, .. } => departed,
            other => panic!("expected DryRun, got {other:?}"),
        };
        let expected_first = sum_store::assignment_v2::chunks_for_archive_v2(
            &ROOT,
            16_385,
            &snapshot.iter().map(|&b| [b; 20]).collect::<Vec<_>>(),
            R,
            &[LEFT; 20],
        )
        .into_iter()
        .next()
        .expect("five archives, R = 3: the departed archive holds chunks");
        assert_eq!(
            departed,
            DepartedAssignee {
                archive: archive(LEFT),
                evidence: AssignedEvidence::LocalFirstChunk(expected_first),
            }
        );
        assert!(rpc.sent().is_empty(), "dry run sends nothing");
    }

    /// Above the cap with no departed assignee. With real hashing a departed
    /// archive in the snapshot of a file this large practically always holds
    /// a chunk, so the oracle is injected; it must be asked about exactly the
    /// departed archives, over the response's snapshot, chunk count and R.
    #[tokio::test]
    async fn above_the_cap_without_a_departed_assignee_is_not_submitted() {
        let cov = coverage_with(
            true,
            20_000,
            vec![
                entry(STAYS, None, true),
                entry(0x32, None, true),
                entry(LEFT, None, false),
            ],
        );
        let rpc = MockRpc::with_coverage(vec![cov]);
        let oracle = FixedOracle::new(None);
        let out = reassign_chunks_v2_with(&rpc, &SEED, &request(false), &oracle).await;
        assert!(
            matches!(out, ReassignOutcome::NoDepartedAssignee { ref departed, .. } if *departed == vec![archive(LEFT)]),
            "got {out:?}"
        );
        rpc.assert_untouched("above the cap, no departed assignee");
        let asked = oracle.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![(
                20_000,
                vec![[STAYS; 20], [0x32; 20], [LEFT; 20]],
                R,
                vec![[LEFT; 20]]
            )]
        );

        // The same response with the oracle reporting a hit is submitted.
        let rpc = MockRpc::with_coverage(vec![rpc.coverage.replies[0].get().unwrap().unwrap()]);
        let oracle = FixedOracle::new(Some((7, [LEFT; 20])));
        let out = reassign_chunks_v2_with(&rpc, &SEED, &request(false), &oracle).await;
        assert!(
            matches!(out, ReassignOutcome::Reassigned { ref departed, .. }
                if departed.evidence == AssignedEvidence::LocalFirstChunk(7)),
            "got {out:?}"
        );
        assert_eq!(rpc.sent().len(), 1);
    }

    /// The chain's count decides whenever it is present; the oracle is asked
    /// only about departed archives the chain left uncounted.
    #[tokio::test]
    async fn chain_counts_decide_before_any_local_computation() {
        let cov = coverage_with(
            true,
            2,
            vec![entry(STAYS, Some(2), true), entry(LEFT, Some(1), false)],
        );
        let rpc = MockRpc::with_coverage(vec![cov]);
        let oracle = FixedOracle::new(None);
        let out = reassign_chunks_v2_with(&rpc, &SEED, &request(true), &oracle).await;
        assert!(matches!(out, ReassignOutcome::DryRun { .. }), "got {out:?}");
        assert!(oracle.asked.lock().unwrap().is_empty());
    }

    /// A block applied during the read: the read is repeated, and the command
    /// refuses after `SNAPSHOT_ATTEMPTS` without one consistent state.
    #[tokio::test]
    async fn height_mismatch_between_reads_is_retried_then_refused() {
        // Every bracket straddles a block.
        let mut rpc = MockRpc::happy();
        rpc.heights = Script::new((10..30).map(Reply::Ok).collect());
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        match out {
            ReassignOutcome::InconsistentSnapshot { attempts, reason } => {
                assert_eq!(attempts, SNAPSHOT_ATTEMPTS);
                assert!(reason.contains("block was applied"), "{reason}");
            }
            other => panic!("expected InconsistentSnapshot, got {other:?}"),
        }
        assert_eq!(rpc.coverage.calls(), SNAPSHOT_ATTEMPTS);
        rpc.assert_untouched("heights never matched");

        // The first bracket straddles a block, the second does not.
        let mut rpc = MockRpc::happy();
        rpc.heights = Script::new(vec![
            Reply::Ok(10),
            Reply::Ok(11),
            Reply::Ok(11),
            Reply::Ok(11),
        ]);
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        assert!(
            matches!(out, ReassignOutcome::DryRun { at_height: 11, .. }),
            "got {out:?}"
        );
        assert_eq!(rpc.coverage.calls(), 2);
    }

    /// Flag set but every archive in the latest snapshot active: the response
    /// mixes two states. It is read again; a consistent read is used.
    #[tokio::test]
    async fn a_self_contradicting_response_is_retried_then_refused() {
        let mixed = coverage_with(
            true,
            2,
            vec![entry(STAYS, Some(2), true), entry(LEFT, Some(1), true)],
        );
        let rpc = MockRpc::with_coverage(vec![mixed.clone()]);
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(
                out,
                ReassignOutcome::InconsistentSnapshot {
                    attempts: SNAPSHOT_ATTEMPTS,
                    ..
                }
            ),
            "got {out:?}"
        );
        assert_eq!(rpc.coverage.calls(), SNAPSHOT_ATTEMPTS);
        rpc.assert_untouched("self-contradicting coverage");

        let rpc = MockRpc::with_coverage(vec![mixed, coverage(true)]);
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        assert!(matches!(out, ReassignOutcome::DryRun { .. }), "got {out:?}");
        assert_eq!(rpc.coverage.calls(), 2);
    }

    /// Responses that cannot support the decision are refused at once.
    #[tokio::test]
    async fn unusable_coverage_is_refused_without_retry() {
        let mut legacy = coverage(true);
        legacy.assignment_epochs.clear();
        legacy.per_epoch.clear();

        let mut short = coverage(true);
        short.assignment_epochs = vec![100, 140];
        short.latest_assignment_epoch = 140;

        let mut wrong_epoch = coverage(true);
        wrong_epoch.per_epoch[0].epoch_height = 99;

        let mut bad_address = coverage(true);
        bad_address.per_epoch[0].per_archive[1].archive = "not-an-address".into();

        for (name, cov) in [
            ("pre-reassignment response", legacy),
            ("per_epoch shorter than the epoch list", short),
            ("last per_epoch entry is not the latest epoch", wrong_epoch),
            ("archive is not an address", bad_address),
        ] {
            let rpc = MockRpc::with_coverage(vec![cov]);
            let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
            assert!(
                matches!(out, ReassignOutcome::UnusableCoverage { .. }),
                "{name}: got {out:?}"
            );
            assert_eq!(rpc.coverage.calls(), 1, "{name}: no retry");
            rpc.assert_untouched(name);
        }

        let mut rpc = MockRpc::happy();
        rpc.params = Reply::Ok(chain_params(0));
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(out, ReassignOutcome::UnusableCoverage { .. }),
            "R = 0: got {out:?}"
        );
        rpc.assert_untouched("R = 0");
    }

    #[tokio::test]
    async fn a_file_owned_by_someone_else_is_never_signed_for() {
        let rpc = MockRpc {
            info: Reply::Ok(Some(file_info(&addr(&OTHER_SEED)))),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(out, ReassignOutcome::NotOwner { .. }),
            "got {out:?}"
        );
        rpc.assert_untouched("foreign owner");
        assert_eq!(rpc.coverage.calls(), 0);
    }

    #[tokio::test]
    async fn an_unregistered_file_is_refused() {
        let rpc = MockRpc {
            info: Reply::Ok(None),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::NotRegistered), "got {out:?}");
        assert!(rpc.sent().is_empty());

        let rpc = MockRpc {
            coverage: Script::new(vec![Reply::Ok(None)]),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::NotRegistered), "got {out:?}");
        assert!(rpc.sent().is_empty());
    }

    /// Every way the builder's output can disagree with the request is
    /// refused before anything is signed.
    #[tokio::test]
    async fn mismatched_builder_output_is_refused_unsigned() {
        let own = addr(&SEED);
        let other = addr(&OTHER_SEED);
        let good = honest_build(&own, ROOT, CHAIN_ID, FEE);
        let abandon = build_response(&TransactionV2 {
            chain_id: CHAIN_ID,
            from: own,
            fee: FEE,
            nonce: NONCE,
            payload: TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
                operation: StorageMetadataOperationV2::AbandonFileV2 {
                    merkle_root: Hash::new(ROOT),
                },
            }),
        });
        let mut trailing = good.clone();
        trailing.unsigned_tx.push_str("00");
        let cases: Vec<(&str, StorageBuildResponse)> = vec![
            ("foreign sender", honest_build(&other, ROOT, CHAIN_ID, FEE)),
            ("other file", honest_build(&own, [0x6b; 32], CHAIN_ID, FEE)),
            ("other chain", honest_build(&own, ROOT, 2, FEE)),
            ("other fee", honest_build(&own, ROOT, CHAIN_ID, FEE + 1)),
            ("other operation", abandon),
            ("trailing bytes", trailing),
            (
                "not hex",
                StorageBuildResponse {
                    unsigned_tx: "0xzz".into(),
                    ..good.clone()
                },
            ),
            (
                "echoed nonce",
                StorageBuildResponse {
                    nonce: NONCE + 1,
                    ..good.clone()
                },
            ),
            (
                "echoed sender",
                StorageBuildResponse {
                    from: other.to_base58(),
                    ..good.clone()
                },
            ),
            (
                "echoed fee",
                StorageBuildResponse {
                    fee: FEE + 1,
                    ..good.clone()
                },
            ),
            (
                "echoed chain id",
                StorageBuildResponse {
                    chain_id: 2,
                    ..good.clone()
                },
            ),
            (
                "signing hash",
                StorageBuildResponse {
                    signing_hash: format!("0x{}", hex::encode([0u8; 32])),
                    ..good.clone()
                },
            ),
        ];
        for (name, resp) in cases {
            let rpc = MockRpc {
                build: Reply::Ok(resp),
                ..MockRpc::happy()
            };
            let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
            assert!(
                matches!(out, ReassignOutcome::BuildMismatch { .. }),
                "{name}: expected BuildMismatch, got {out:?}"
            );
            assert!(rpc.sent().is_empty(), "{name}: nothing may be sent");
        }
    }

    #[tokio::test]
    async fn rpc_errors_fail_closed_at_every_step() {
        let cases = [
            MockRpc {
                info: Reply::Err("file info down"),
                ..MockRpc::happy()
            },
            MockRpc {
                params: Reply::Err("chain params down"),
                ..MockRpc::happy()
            },
            MockRpc {
                heights: Script::new(vec![Reply::Err("height down")]),
                ..MockRpc::happy()
            },
            MockRpc {
                heights: Script::new(vec![Reply::Ok(500), Reply::Err("height down after")]),
                ..MockRpc::happy()
            },
            MockRpc {
                coverage: Script::new(vec![Reply::Err("coverage down")]),
                ..MockRpc::happy()
            },
            MockRpc {
                build: Reply::Err("builder down"),
                ..MockRpc::happy()
            },
        ];
        for rpc in cases {
            let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
            assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
            assert!(rpc.sent().is_empty());
        }

        let rpc = MockRpc {
            send: Reply::Err("submission refused by the node"),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
    }

    #[tokio::test]
    async fn dry_run_verifies_but_never_signs_or_sends() {
        let rpc = MockRpc::happy();
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        match out {
            ReassignOutcome::DryRun {
                nonce,
                fee,
                signing_hash,
                departed,
                at_height,
            } => {
                assert_eq!(nonce, NONCE);
                assert_eq!(fee, FEE);
                assert_eq!(
                    signing_hash,
                    honest_build(&addr(&SEED), ROOT, CHAIN_ID, FEE).signing_hash
                );
                assert_eq!(departed.archive, archive(LEFT));
                assert_eq!(at_height, 500);
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        assert_eq!(rpc.built(), 1, "dry run still verifies the built bytes");
        assert!(rpc.sent().is_empty(), "dry run sends nothing");
    }

    #[tokio::test]
    async fn dry_run_still_refuses_mismatched_builder_output() {
        let rpc = MockRpc {
            build: Reply::Ok(honest_build(&addr(&OTHER_SEED), ROOT, CHAIN_ID, FEE)),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        assert!(
            matches!(out, ReassignOutcome::BuildMismatch { .. }),
            "got {out:?}"
        );
    }

    #[tokio::test]
    async fn a_chain_refusal_is_reported_with_its_reason() {
        let rpc = MockRpc {
            status: TxStatusV2::Failed {
                block_height: Some(778),
                reason: "Failed(332): reassignment not needed".into(),
            },
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        match out {
            ReassignOutcome::Refused {
                tx_hash,
                block_height,
                reason,
            } => {
                assert_eq!(tx_hash, "0xfeed");
                assert_eq!(block_height, Some(778));
                assert!(reason.contains("332"), "reason keeps the code: {reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    /// N2 over coverage the CHAIN produced (sum-store's chain-generated
    /// vectors; see `crates/sum-store/tests/n2_assignment_vectors.rs`): the
    /// verdict, and where it comes from, match the chain's own answer at and
    /// above the 16,384-chunk cap, including a latest epoch that is a
    /// reassignment.
    #[test]
    fn n2_on_chain_generated_coverage_matches_the_chain() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sum-store/tests/data/n2_assignment_vectors/n2_assignment_vectors.json"
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("vectors")).unwrap();
        let r = doc["replication_factor"].as_u64().unwrap() as u32;
        let cases = doc["cases"].as_array().unwrap();
        assert!(cases.len() >= 5);
        for c in cases {
            let label = c["label"].as_str().unwrap();
            let epochs: Vec<u64> = serde_json::from_value(c["assignment_epochs"].clone()).unwrap();
            let latest: Vec<serde_json::Value> = c["latest_per_archive"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "archive": e["archive"],
                        "assigned_count": e["rpc_assigned_count"],
                        "attested_count": 0,
                        "currently_active": e["currently_active"],
                    })
                })
                .collect();
            // Earlier epochs are not read by N2; they carry empty lists.
            let mut per_epoch: Vec<serde_json::Value> = epochs[..epochs.len() - 1]
                .iter()
                .map(|h| serde_json::json!({"epoch_height": h, "is_epoch_zero": false, "covered_count": 0, "per_archive": []}))
                .collect();
            per_epoch.push(serde_json::json!({
                "epoch_height": epochs.last().unwrap(),
                "is_epoch_zero": epochs.len() == 1,
                "covered_count": 0,
                "per_archive": latest,
            }));
            let chunk_count = c["chunk_count"].as_u64().unwrap() as u32;
            let cov: AssignmentCoverageV2 = serde_json::from_value(serde_json::json!({
                "chunk_count": chunk_count,
                "covered_count": 0,
                "can_activate_now": false,
                "missing_total": chunk_count,
                "missing_offset": 0,
                "missing_indices": [],
                "per_archive": [],
                "assignment_epochs": epochs,
                "latest_assignment_epoch": epochs.last().unwrap(),
                "reassignment_needed": c["reassignment_needed"],
                "per_epoch": per_epoch,
            }))
            .unwrap();
            let root: [u8; 32] = hex::decode(c["merkle_root"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();

            let verdict = evaluate_n2(&cov, &root, r, &ChainScorer).expect(label);
            let expected = c["departed_assignee"].as_bool().unwrap();
            match verdict {
                N2Verdict::Departed(d) => {
                    assert!(expected, "{label}: N2 found {d:?}, the chain found none");
                    match d.evidence {
                        AssignedEvidence::ChainCount(n) => {
                            assert!(chunk_count <= 16_384, "{label}");
                            assert!(n > 0, "{label}");
                        }
                        AssignedEvidence::LocalFirstChunk(_) => {
                            assert!(chunk_count > 16_384, "{label}");
                        }
                    }
                }
                N2Verdict::NoDepartedAssignee { departed } => {
                    assert!(!expected, "{label}: N2 found none, the chain found one");
                    assert!(!departed.is_empty(), "{label}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_dropped_transaction_is_a_failure_not_a_success() {
        let rpc = MockRpc {
            status: TxStatusV2::Dropped,
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
    }
}
