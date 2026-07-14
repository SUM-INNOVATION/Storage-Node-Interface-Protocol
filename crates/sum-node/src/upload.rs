//! Upload orchestrator — pushes chunks to R assigned nodes with confirmation.
//!
//! Instead of pushing to a single peer and relying on MarketSync for
//! replication, the UploadOrchestrator pushes each chunk directly to its
//! R=3 assigned nodes in parallel and waits for confirmation via gossipsub
//! ChunkAnnouncements or ShardResponse ACKs.
//!
//! This eliminates the single-point-of-failure window between upload and
//! replication that exists in the current `ingest` flow.
//!
//! Memory model: each chunk is read into a single `Arc<[u8]>` buffer that
//! is cheaply cloned across replicas. The full byte payload is materialized
//! exactly once, inside the swarm command handler, just before libp2p
//! serializes the request. Chunks are processed in bounded slices of
//! `max_in_flight_chunks` so the peak number of unique chunk buffers held
//! at any moment is bounded by `max_in_flight_chunks`, regardless of the
//! file's total chunk count.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use tracing::{info, warn};

use sum_net::identity;
use sum_net::{PeerId, SumNet, SumNetEvent};
use sum_store::{SumStore, compute_chunk_assignment, nodes_for_chunk};
use sum_types::rpc_types::NodeRecordInfo;
use sum_types::storage::DataManifest;

use crate::rpc_client::L1RpcClient;

// ── UploadNet trait ──────────────────────────────────────────────────────────

/// Abstraction over the two `SumNet` operations the upload orchestrator
/// needs: pushing a chunk and pulling the next event. Lets tests inject a
/// mock without spinning up a libp2p swarm.
#[async_trait]
pub trait UploadNet: Send + Sync {
    /// Send a push request for `cid` to `peer_id`, sharing the underlying
    /// buffer via `Arc<[u8]>`. Multiple replicas of the same chunk can clone
    /// the same `Arc<[u8]>` cheaply.
    async fn push_chunk_shared(&self, peer_id: PeerId, cid: String, data: Arc<[u8]>) -> Result<()>;

    /// Pull the next network event from the swarm. Returns `None` when the
    /// underlying network has shut down.
    async fn next_event(&self) -> Option<SumNetEvent>;
}

#[async_trait]
impl UploadNet for SumNet {
    async fn push_chunk_shared(&self, peer_id: PeerId, cid: String, data: Arc<[u8]>) -> Result<()> {
        SumNet::push_chunk_shared(self, peer_id, cid, data).await
    }

    async fn next_event(&self) -> Option<SumNetEvent> {
        SumNet::next_event(self).await
    }
}

// ── Public Types ─────────────────────────────────────────────────────────────

/// Default cap on the number of chunks that may have outstanding push
/// requests at any given moment. With `R=3` replicas, the peak number of
/// queued `SwarmCommand::PushShard` entries is bounded by
/// `DEFAULT_MAX_IN_FLIGHT_CHUNKS * effective_r`.
pub const DEFAULT_MAX_IN_FLIGHT_CHUNKS: usize = 4;

/// Orchestrates uploading a file's chunks to R assigned nodes.
pub struct UploadOrchestrator {
    rpc: Arc<L1RpcClient>,
    timeout: Duration,
    max_in_flight_chunks: usize,
    /// Live-chain replication factor sourced from
    /// `ChainParamsInfo::assignment_replication_factor` at operation
    /// entry. Used to derive the per-chunk plan; the plan is what
    /// success is measured against. Effective per-chunk cardinality is
    /// `min(replication_factor, eligible_snapshot.len())`.
    replication_factor: u32,
}

/// Result of an upload operation.
///
/// `confirmed` and `total` are aggregate (chunk, node) ACK counts kept for
/// observability. The success criterion is **per-chunk**: every chunk must
/// reach R replica ACKs. Use [`UploadResult::check_success`] to test it.
#[derive(Debug, Clone)]
pub struct UploadResult {
    /// Number of (chunk, node) push confirmations received.
    pub confirmed: u32,
    /// Total (chunk, node) pushes attempted.
    pub total: u32,
    /// Whether the timeout was reached before all confirmations.
    pub timeout: bool,
    /// Details of failed pushes.
    pub failed: Vec<FailedPush>,
    /// Per-chunk ACK count, indexed by `chunk_index`. Length equals the
    /// manifest's `chunk_count`. `per_chunk_confirmations[i]` is the number
    /// of distinct replica ACKs for chunk `i`.
    pub per_chunk_confirmations: Vec<u32>,
    /// Number of chunks that reached the replication threshold:
    /// `chunks_fully_confirmed = |{ i : per_chunk_confirmations[i] >= R }|`.
    /// Treated as a cached hint by [`Self::check_success`], which
    /// recomputes the verdict from `per_chunk_confirmations`.
    pub chunks_fully_confirmed: u32,
    /// Distinct peers that ACKed at least one chunk. Used by callers to
    /// know who to push the manifest to so those archives can resolve
    /// `cid → root` for ACL purposes.
    pub chunk_recipients: HashSet<PeerId>,
    /// Per-chunk **planned** target count derived at planning time from
    /// `assigned_archives(..., min(configured_r, eligible_snapshot.len()))`.
    /// The success check uses `per_chunk_confirmations[i] >=
    /// per_chunk_expected[i]` — bare `configured_r` is never
    /// re-consulted. On R > N deployments the plan correctly requires
    /// only N ACKs per chunk, matching chain-side aggregate coverage
    /// (upstream `sum-chain/crates/state/src/storage_metadata.rs`).
    pub per_chunk_expected: Vec<u32>,
    /// The `assignment_replication_factor` the orchestrator was
    /// constructed with, recorded at planning time. `NoEligibleTargets`
    /// reports this value truthfully (rather than inferring 0 from an
    /// empty per-chunk vector).
    pub configured_r: u32,
    /// The length of the `filter_active_archives`-narrowed snapshot
    /// used to build the per-chunk assignment. `NoEligibleTargets`
    /// reports this value truthfully.
    pub eligible_snapshot_len: usize,
}

/// A single failed push attempt.
#[derive(Debug, Clone)]
pub struct FailedPush {
    pub chunk_index: u32,
    pub cid: String,
    pub error: String,
}

/// Why an upload should be considered unsuccessful. Distinct from a
/// transport-level `Err` from the orchestrator: this is the post-run
/// verdict against the per-chunk replication target.
///
/// `NoEligibleTargets` is emitted at the planning boundary
/// (`UploadOrchestrator::run` / `run_with_nodes`) with the real
/// `configured_r` and `eligible_snapshot_len` that were used to build
/// the plan — the values are never inferred from a per-chunk vector.
#[derive(Debug, thiserror::Error)]
pub enum UploadFailure {
    #[error("upload timed out before all chunks reached the replication target")]
    Timeout,
    #[error("{count} chunk push(es) failed at the transport layer")]
    FailedPushes {
        count: usize,
        pushes: Vec<FailedPush>,
    },
    #[error(
        "only {fully_confirmed_chunks}/{expected_chunks} chunks reached their planned \
         per-chunk target ACK count; under-replicated indices: {under_replicated:?}"
    )]
    IncompleteConfirmations {
        expected_chunks: u32,
        fully_confirmed_chunks: u32,
        under_replicated: Vec<u32>,
    },
    #[error(
        "no eligible push targets — the effective per-chunk target set was empty for \
         every chunk (configured_r={configured_r}, eligible_snapshot_len={eligible_snapshot_len}). \
         No push was dispatched. Refuse to declare success."
    )]
    NoEligibleTargets {
        /// The `assignment_replication_factor` this orchestrator was
        /// constructed with — captured at planning time from
        /// `sum_node::runtime_params::RuntimeChainParams`, not inferred.
        configured_r: u32,
        /// The size of the `filter_active_archives`-narrowed snapshot
        /// used to build the per-chunk assignment — captured at
        /// planning time, not inferred.
        eligible_snapshot_len: usize,
    },
}

impl UploadResult {
    /// Plan-derived success check (#33). For each chunk, verify the
    /// number of received ACKs is at least the planned per-chunk
    /// target set size (`per_chunk_expected[i]`, computed at planning
    /// time from `assigned_archives(..., min(configured_r, N))`).
    /// Configured `R` is never re-consulted here — the plan is the
    /// authority so `R > N` deployments correctly require only `N`
    /// ACKs per chunk.
    ///
    /// This function does NOT emit `UploadFailure::NoEligibleTargets`.
    /// The empty-target case is caught at the planning boundary inside
    /// `UploadOrchestrator::run` / `run_with_nodes` where the real
    /// `configured_r` and `eligible_snapshot_len` are known; the
    /// orchestrator returns `Err(UploadFailure::NoEligibleTargets { .. })`
    /// with those real values before dispatching any push. Reaching
    /// this method already means at least one push was dispatched.
    pub fn check_success(&self) -> Result<(), UploadFailure> {
        // Truthful `NoEligibleTargets`: emitted at the planning
        // boundary when either R=0 or the eligible snapshot was empty.
        // Reports the ACTUAL `configured_r` and `eligible_snapshot_len`
        // from struct fields (populated at planning time in
        // `UploadOrchestrator::run` / `run_with_nodes`). Never
        // fabricated from an all-zero `per_chunk_expected` vector.
        if self.configured_r == 0 || self.eligible_snapshot_len == 0 {
            return Err(UploadFailure::NoEligibleTargets {
                configured_r: self.configured_r,
                eligible_snapshot_len: self.eligible_snapshot_len,
            });
        }
        if self.timeout {
            return Err(UploadFailure::Timeout);
        }
        if !self.failed.is_empty() {
            return Err(UploadFailure::FailedPushes {
                count: self.failed.len(),
                pushes: self.failed.clone(),
            });
        }
        let expected_chunks = self.per_chunk_confirmations.len() as u32;
        let under_replicated: Vec<u32> = self
            .per_chunk_confirmations
            .iter()
            .zip(self.per_chunk_expected.iter())
            .enumerate()
            .filter_map(|(i, (&count, &expected))| {
                if count < expected {
                    Some(i as u32)
                } else {
                    None
                }
            })
            .collect();
        let fully_confirmed_chunks = expected_chunks - under_replicated.len() as u32;
        if !under_replicated.is_empty() {
            return Err(UploadFailure::IncompleteConfirmations {
                expected_chunks,
                fully_confirmed_chunks,
                under_replicated,
            });
        }
        Ok(())
    }
}

// ── Implementation ───────────────────────────────────────────────────────────

impl UploadOrchestrator {
    pub fn new(rpc: Arc<L1RpcClient>, timeout: Duration, replication_factor: u32) -> Self {
        Self {
            rpc,
            timeout,
            max_in_flight_chunks: DEFAULT_MAX_IN_FLIGHT_CHUNKS,
            replication_factor,
        }
    }

    /// Override the default `max_in_flight_chunks` cap.
    /// Values < 1 are clamped to 1.
    pub fn with_max_in_flight_chunks(mut self, n: usize) -> Self {
        self.max_in_flight_chunks = n.max(1);
        self
    }

    /// Fetch the active-archive snapshot pinned to the current finalized
    /// head.
    ///
    /// Reads the finalized height exactly once via
    /// `chain_get_block_height` (which passes `["finalized"]`), then the
    /// height-pinned snapshot via `storage_get_active_nodes_at_height` at
    /// exactly that height. This is the supported replacement for the
    /// removed `storage_getActiveNodes` bulk endpoint. Both lookups
    /// surface their typed errors; the caller (`run`) propagates them
    /// with `?`.
    async fn active_nodes_at_finalized_head(&self) -> Result<Vec<NodeRecordInfo>> {
        let finalized_height = self.rpc.chain_get_block_height().await?.height;
        self.rpc
            .storage_get_active_nodes_at_height(finalized_height)
            .await
    }

    /// Push all chunks in the manifest to their assigned nodes.
    ///
    /// Returns when all pushes are confirmed (ACK responses received)
    /// or when the timeout is reached.
    pub async fn run<N: UploadNet + ?Sized>(
        &self,
        net: &N,
        store: &SumStore,
        manifest: &DataManifest,
        peer_addresses: &HashMap<PeerId, [u8; 20]>,
    ) -> Result<UploadResult> {
        // Fetch active node directory from L1 and narrow to
        // exactly-eligible archives per the shared eligibility contract
        // (`role == "ArchiveNode" && status == "Active"`). Any
        // Slashed/Unbonding/Withdrawn/unknown-future or Validator record
        // is dropped before address decode.
        //
        // The active-node snapshot is pinned to the finalized head: the
        // finalized height is read exactly once at the top of the
        // operation (inside `active_nodes_at_finalized_head`). A
        // height-lookup failure propagates as this operation's typed error
        // (`?`), the same as any other RPC failure.
        let node_records = self.active_nodes_at_finalized_head().await?;
        let node_records = sum_types::rpc_types::filter_active_archives(node_records);
        let mut node_addrs: Vec<[u8; 20]> = Vec::new();
        for record in &node_records {
            if let Ok(addr) = identity::l1_address_from_base58(&record.address) {
                node_addrs.push(addr);
            }
        }
        node_addrs.sort();
        // Route empty-snapshot through the unified NoEligibleTargets
        // contract so both R=0 (with any N) and R>0 with N=0 surface
        // with truthful (configured_r, eligible_snapshot_len) at the
        // CLI mapper. No push is dispatched.
        self.run_with_nodes(net, store, manifest, peer_addresses, &node_addrs)
            .await
    }

    /// Run the upload pipeline against a precomputed list of active node
    /// L1 addresses. Exposed so tests (and any caller that already knows
    /// the node directory) can bypass the L1 RPC.
    pub async fn run_with_nodes<N: UploadNet + ?Sized>(
        &self,
        net: &N,
        store: &SumStore,
        manifest: &DataManifest,
        peer_addresses: &HashMap<PeerId, [u8; 20]>,
        node_addrs: &[[u8; 20]],
    ) -> Result<UploadResult> {
        let chunk_count = manifest.chunk_count as u64;
        let eligible_snapshot_len = node_addrs.len();
        let configured_r = self.replication_factor;

        // Planning boundary: if `configured_r == 0` OR the eligible
        // snapshot is empty, no push is dispatchable. Return an empty
        // `UploadResult` populated with the REAL `configured_r` and
        // `eligible_snapshot_len` — `check_success()` maps this to
        // `UploadFailure::NoEligibleTargets { .. }` with the truthful
        // values. No push is enqueued; the network is untouched.
        if configured_r == 0 || eligible_snapshot_len == 0 {
            info!(
                configured_r,
                eligible_snapshot_len,
                "upload planning: no eligible targets — refusing to dispatch any push"
            );
            return Ok(UploadResult {
                confirmed: 0,
                total: 0,
                timeout: false,
                failed: Vec::new(),
                per_chunk_confirmations: vec![0; manifest.chunk_count as usize],
                chunks_fully_confirmed: 0,
                chunk_recipients: HashSet::new(),
                per_chunk_expected: vec![0; manifest.chunk_count as usize],
                configured_r,
                eligible_snapshot_len,
            });
        }

        let assignment =
            compute_chunk_assignment(&manifest.merkle_root, chunk_count, node_addrs, configured_r);
        // Plan-derived per-chunk expected ACK counts. Each chunk's
        // expected count is the size of its assigned set, which is
        // `min(configured_r, eligible_snapshot_len)` per upstream
        // `sum-chain/crates/primitives/src/storage_metadata.rs:299`.
        let per_chunk_expected: Vec<u32> = assignment.iter().map(|a| a.len() as u32).collect();

        // Reverse map: L1 address -> PeerId
        let mut addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();
        for (&pid, &addr) in peer_addresses.iter() {
            addr_to_peer.insert(addr, pid);
        }

        // CID -> chunk_index lookup for per-chunk confirmation accounting.
        // Built once from the manifest; ACKs come back keyed by CID, and we
        // need the chunk_index to bump the right slot in
        // `per_chunk_confirmations`.
        let mut cid_to_index: HashMap<String, u32> = HashMap::new();
        for chunk in &manifest.chunks {
            cid_to_index.insert(chunk.cid.clone(), chunk.chunk_index);
        }

        let mut total_pushes: u32 = 0;
        let mut confirmed: u32 = 0;
        let mut failed: Vec<FailedPush> = Vec::new();
        let mut per_chunk_confirmations: Vec<u32> = vec![0; manifest.chunk_count as usize];
        let mut chunk_recipients: HashSet<PeerId> = HashSet::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut timed_out = false;

        // Process chunks in bounded slices so peak unique-buffer count is
        // bounded by `max_in_flight_chunks`, independent of total chunk count.
        'outer: for slice in manifest.chunks.chunks(self.max_in_flight_chunks) {
            // Per-slice pending ACK set so we drain ACKs before starting the
            // next slice. Bounded by `max_in_flight_chunks * R`.
            let mut slice_pending: HashSet<(String, PeerId)> = HashSet::new();

            // ── Phase 1 (per slice): read each chunk into one Arc<[u8]>
            //                        and fan out R replica push commands ──
            for chunk in slice {
                let assigned = match nodes_for_chunk(&assignment, chunk.chunk_index) {
                    Some(a) => a,
                    None => continue,
                };

                // Single read from disk per chunk; wrap once in Arc<[u8]>.
                let raw = store
                    .local
                    .get(&chunk.cid)
                    .map_err(|e| anyhow::anyhow!("missing chunk {}: {e}", chunk.cid))?;
                let data: Arc<[u8]> = Arc::from(raw.into_boxed_slice());

                for node_addr in assigned {
                    let Some(&peer_id) = addr_to_peer.get(node_addr) else {
                        failed.push(FailedPush {
                            chunk_index: chunk.chunk_index,
                            cid: chunk.cid.clone(),
                            error: format!("no PeerId for node {}", hex::encode(node_addr)),
                        });
                        total_pushes += 1;
                        continue;
                    };

                    // Cheap pointer-bump clone — all R replicas share the
                    // same backing buffer.
                    match net
                        .push_chunk_shared(peer_id, chunk.cid.clone(), Arc::clone(&data))
                        .await
                    {
                        Ok(()) => {
                            slice_pending.insert((chunk.cid.clone(), peer_id));
                            total_pushes += 1;
                        }
                        Err(e) => {
                            failed.push(FailedPush {
                                chunk_index: chunk.chunk_index,
                                cid: chunk.cid.clone(),
                                error: e.to_string(),
                            });
                            total_pushes += 1;
                        }
                    }
                }
                // `data` (the local Arc handle) drops here. Queued commands
                // still hold their Arc clones, keeping the buffer alive
                // until the swarm loop processes each one.
            }

            info!(
                slice_size = slice.len(),
                pending_acks = slice_pending.len(),
                "slice push requests sent — waiting for ACKs before next slice"
            );

            // ── Phase 2 (per slice): drain ACKs before starting next slice ──
            while !slice_pending.is_empty() {
                tokio::select! {
                    event = net.next_event() => {
                        match event {
                            Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                                if response.error.is_none() {
                                    if slice_pending.remove(&(response.cid.clone(), peer_id)) {
                                        confirmed += 1;
                                        if let Some(&idx) = cid_to_index.get(&response.cid) {
                                            per_chunk_confirmations[idx as usize] += 1;
                                        }
                                        chunk_recipients.insert(peer_id);
                                        info!(
                                            cid = %response.cid,
                                            %peer_id,
                                            confirmed,
                                            remaining = slice_pending.len(),
                                            "push confirmed"
                                        );
                                    }
                                } else if let Some(ref err) = response.error {
                                    if slice_pending.remove(&(response.cid.clone(), peer_id)) {
                                        warn!(
                                            cid = %response.cid,
                                            %peer_id,
                                            %err,
                                            "push rejected by peer"
                                        );
                                        let chunk_index = cid_to_index
                                            .get(&response.cid)
                                            .copied()
                                            .unwrap_or(u32::MAX);
                                        failed.push(FailedPush {
                                            chunk_index,
                                            cid: response.cid.clone(),
                                            error: err.clone(),
                                        });
                                    }
                                }
                            }
                            None => {
                                bail!("network shut down while waiting for push confirmations");
                            }
                            _ => {}
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        warn!(
                            unconfirmed = slice_pending.len(),
                            "upload timeout — not all pushes confirmed"
                        );
                        timed_out = true;
                        // Record remaining pending in this slice as failures.
                        for (cid, peer_id) in slice_pending.drain() {
                            let chunk_index = cid_to_index.get(&cid).copied().unwrap_or(u32::MAX);
                            failed.push(FailedPush {
                                chunk_index,
                                cid,
                                error: format!("timeout — no ACK from {peer_id}"),
                            });
                        }
                        break 'outer;
                    }
                }
            }
        }

        // Plan-derived hint: chunks that reached their planned target.
        let chunks_fully_confirmed = per_chunk_confirmations
            .iter()
            .zip(per_chunk_expected.iter())
            .filter(|(conf, exp)| **conf >= **exp)
            .count() as u32;

        Ok(UploadResult {
            confirmed,
            total: total_pushes,
            timeout: timed_out,
            failed,
            per_chunk_confirmations,
            chunks_fully_confirmed,
            chunk_recipients,
            per_chunk_expected,
            configured_r,
            eligible_snapshot_len,
        })
    }
}

// ── Inline unit tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::storage::ChunkDescriptor;

    /// Verify that cloning an `Arc<[u8]>` shares the underlying buffer
    /// rather than copying it. This is the foundational property the
    /// upload memory fix relies on.
    #[test]
    fn arc_fanout_shares_buffer() {
        let payload = vec![0xAB; 4096];
        let original_ptr = payload.as_ptr();
        let shared: Arc<[u8]> = Arc::from(payload.into_boxed_slice());
        let shared_ptr = shared.as_ptr();

        // Cloning Arc<[u8]> R times should NOT copy the bytes.
        let r1 = Arc::clone(&shared);
        let r2 = Arc::clone(&shared);
        let r3 = Arc::clone(&shared);

        // All replicas point at the same backing buffer.
        assert_eq!(shared.as_ptr(), r1.as_ptr());
        assert_eq!(shared.as_ptr(), r2.as_ptr());
        assert_eq!(shared.as_ptr(), r3.as_ptr());

        // Strong count is original + 3 clones = 4.
        assert_eq!(Arc::strong_count(&shared), 4);

        // After the original Vec was consumed, the Arc owns the buffer
        // (the pointer may differ from the Vec's because Box converts).
        // The important invariant: all clones share one pointer.
        let _ = original_ptr;
        let _ = shared_ptr;
    }

    /// Verify that dropping replica clones decrements the refcount and
    /// the buffer is freed exactly once when the last clone drops.
    #[test]
    fn arc_fanout_releases_when_all_clones_drop() {
        let shared: Arc<[u8]> = Arc::from(vec![0u8; 1024].into_boxed_slice());
        assert_eq!(Arc::strong_count(&shared), 1);

        let r1 = Arc::clone(&shared);
        let r2 = Arc::clone(&shared);
        assert_eq!(Arc::strong_count(&shared), 3);

        drop(r1);
        drop(r2);
        assert_eq!(Arc::strong_count(&shared), 1);
    }

    /// Verify that `chunks(M)` slicing produces the expected bounded groups.
    /// The orchestrator relies on this to bound peak in-flight chunk count.
    #[test]
    fn slice_plan_respects_max_in_flight() {
        fn dummy(idx: u32) -> ChunkDescriptor {
            ChunkDescriptor {
                chunk_index: idx,
                offset: 0,
                size: 0,
                blake3_hash: [0u8; 32],
                cid: format!("cid{idx}"),
                plaintext_blake3_hash: None,
            }
        }
        let chunks: Vec<ChunkDescriptor> = (0..10).map(dummy).collect();

        let slices: Vec<&[ChunkDescriptor]> = chunks.chunks(4).collect();
        assert_eq!(slices.len(), 3);
        assert_eq!(slices[0].len(), 4);
        assert_eq!(slices[1].len(), 4);
        assert_eq!(slices[2].len(), 2);

        // No slice ever exceeds the max.
        for slice in &slices {
            assert!(slice.len() <= 4);
        }
    }

    #[test]
    fn slice_plan_exact_multiple() {
        let n = 12;
        let m = 4;
        let count = (0..n).count();
        let chunks: Vec<u32> = (0..count as u32).collect();
        let slices: Vec<&[u32]> = chunks.chunks(m).collect();
        assert_eq!(slices.len(), 3);
        for s in &slices {
            assert_eq!(s.len(), 4);
        }
    }

    #[test]
    fn slice_plan_smaller_than_max() {
        let chunks: Vec<u32> = (0..2).collect();
        let slices: Vec<&[u32]> = chunks.chunks(4).collect();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].len(), 2);
    }

    #[test]
    fn with_max_in_flight_clamps_zero_to_one() {
        let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
        let orch =
            UploadOrchestrator::new(rpc, Duration::from_secs(1), 3).with_max_in_flight_chunks(0);
        assert_eq!(orch.max_in_flight_chunks, 1);
    }

    #[test]
    fn default_max_in_flight_is_set() {
        let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
        let orch = UploadOrchestrator::new(rpc, Duration::from_secs(1), 3);
        assert_eq!(orch.max_in_flight_chunks, DEFAULT_MAX_IN_FLIGHT_CHUNKS);
    }

    // ── UploadResult::check_success matrix (plan-derived, #33) ────────────

    /// Build an UploadResult whose per-chunk expected equals R (i.e. an
    /// R<=N scenario) — the plan-derived check compares ACKs against
    /// this expected vector, not any bare configured R.
    fn make_result(
        per_chunk: Vec<u32>,
        timeout: bool,
        failed: Vec<FailedPush>,
        replication_factor: u32,
    ) -> UploadResult {
        let per_chunk_expected: Vec<u32> = vec![replication_factor; per_chunk.len()];
        let chunks_fully_confirmed = per_chunk
            .iter()
            .zip(per_chunk_expected.iter())
            .filter(|(c, e)| **c >= **e)
            .count() as u32;
        let confirmed: u32 = per_chunk.iter().sum();
        UploadResult {
            confirmed,
            total: confirmed + failed.len() as u32,
            timeout,
            failed,
            per_chunk_confirmations: per_chunk,
            chunks_fully_confirmed,
            chunk_recipients: HashSet::new(),
            per_chunk_expected,
            // A successful path implies R > 0 and eligible_snapshot_len > 0.
            configured_r: replication_factor,
            eligible_snapshot_len: replication_factor as usize,
        }
    }

    /// Build an R>N scenario: configured R=5 with only N=3 eligible
    /// archives → plan says expected=3 per chunk (effective R via
    /// upstream `min(R, N)`). Records the real `configured_r` and
    /// `eligible_snapshot_len` so `NoEligibleTargets`-style diagnostics
    /// carry truthful values.
    fn make_result_effective(
        per_chunk: Vec<u32>,
        configured_r: u32,
        eligible_n: usize,
        timeout: bool,
        failed: Vec<FailedPush>,
    ) -> UploadResult {
        let effective_r = std::cmp::min(configured_r as usize, eligible_n) as u32;
        let per_chunk_expected: Vec<u32> = vec![effective_r; per_chunk.len()];
        let chunks_fully_confirmed = per_chunk
            .iter()
            .zip(per_chunk_expected.iter())
            .filter(|(c, e)| **c >= **e)
            .count() as u32;
        let confirmed: u32 = per_chunk.iter().sum();
        UploadResult {
            confirmed,
            total: confirmed + failed.len() as u32,
            timeout,
            failed,
            per_chunk_confirmations: per_chunk,
            chunks_fully_confirmed,
            chunk_recipients: HashSet::new(),
            per_chunk_expected,
            configured_r,
            eligible_snapshot_len: eligible_n,
        }
    }

    /// Every chunk fully replicated to R archives → check_success returns Ok.
    #[test]
    fn check_success_happy_path_full_replication() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let result = make_result(vec![r, r, r, r, r], false, vec![], r);
        assert!(result.check_success().is_ok());
    }

    /// Exactly one chunk receives only R-1 ACKs → IncompleteConfirmations
    /// with that chunk's index reported.
    #[test]
    fn check_success_under_replicates_single_chunk() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let mut per_chunk = vec![r; 10];
        per_chunk[5] = r - 1;
        let result = make_result(per_chunk, false, vec![], r);
        match result.check_success() {
            Err(UploadFailure::IncompleteConfirmations {
                expected_chunks,
                fully_confirmed_chunks,
                under_replicated,
            }) => {
                assert_eq!(expected_chunks, 10);
                assert_eq!(fully_confirmed_chunks, 9);
                assert_eq!(under_replicated, vec![5]);
            }
            other => panic!("expected IncompleteConfirmations, got {other:?}"),
        }
    }

    /// Timeout dominates: even if some chunks are fully confirmed, a timeout
    /// flag means the result is `Err(Timeout)`.
    #[test]
    fn check_success_timeout_dominates() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let result = make_result(vec![r, r, 0, 0], true, vec![], r);
        assert!(matches!(
            result.check_success(),
            Err(UploadFailure::Timeout)
        ));
    }

    /// Any FailedPush in the result is fatal regardless of confirmation
    /// counts — the orchestrator already knows something went wrong.
    #[test]
    fn check_success_failed_push_is_fatal() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let failed = vec![FailedPush {
            chunk_index: 7,
            cid: "bafk_test_chunk7".to_string(),
            error: "store write failed".to_string(),
        }];
        let result = make_result(vec![r; 10], false, failed.clone(), r);
        match result.check_success() {
            Err(UploadFailure::FailedPushes { count, pushes }) => {
                assert_eq!(count, 1);
                assert_eq!(pushes.len(), 1);
                assert_eq!(pushes[0].chunk_index, 7);
            }
            other => panic!("expected FailedPushes, got {other:?}"),
        }
    }

    /// `check_success` recomputes the verdict from
    /// `per_chunk_confirmations` and `per_chunk_expected` — a stale
    /// `chunks_fully_confirmed` cache does not affect the outcome.
    #[test]
    fn check_success_recomputes_fully_confirmed_ignoring_cache() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let mut per_chunk = vec![r; 4];
        per_chunk[2] = r - 1;
        let per_chunk_expected: Vec<u32> = vec![r; 4];
        let result = UploadResult {
            confirmed: per_chunk.iter().sum(),
            total: 4 * r,
            timeout: false,
            failed: vec![],
            per_chunk_confirmations: per_chunk,
            chunks_fully_confirmed: 4, // ← stale / forged cache
            chunk_recipients: HashSet::new(),
            per_chunk_expected,
            configured_r: r,
            eligible_snapshot_len: r as usize,
        };
        match result.check_success() {
            Err(UploadFailure::IncompleteConfirmations {
                fully_confirmed_chunks,
                under_replicated,
                ..
            }) => {
                assert_eq!(fully_confirmed_chunks, 3);
                assert_eq!(under_replicated, vec![2]);
            }
            other => panic!("expected IncompleteConfirmations, got {other:?}"),
        }
    }

    /// Mixed under-replication produces a precise list of bad indices in
    /// ascending order.
    #[test]
    fn check_success_mixed_under_replication_indices() {
        let r = sum_types::storage::DEFAULT_REPLICATION_FACTOR;
        let per_chunk = vec![r - 1, r, r, r - 2, r, r, r, r, 0, r];
        let result = make_result(per_chunk, false, vec![], r);
        match result.check_success() {
            Err(UploadFailure::IncompleteConfirmations {
                under_replicated, ..
            }) => {
                assert_eq!(under_replicated, vec![0, 3, 8]);
            }
            other => panic!("expected IncompleteConfirmations, got {other:?}"),
        }
    }

    // ── Effective-R (#33) success matrix ──────────────────────────────────

    /// R=5, N=3 → per-chunk plan target = min(5, 3) = 3. All 3
    /// planned targets ACK per chunk → success.
    #[test]
    fn upload_r_5_n_3_all_three_assigned_ack_succeeds() {
        let result = make_result_effective(vec![3; 4], 5, 3, false, vec![]);
        assert!(result.check_success().is_ok());
    }

    /// R=5, N=3 → per-chunk plan target = 3. Only 2 of 3 planned
    /// targets ACK per chunk → failure with every under-replicated
    /// index reported.
    #[test]
    fn upload_r_5_n_3_two_of_three_assigned_ack_fails() {
        let result = make_result_effective(vec![2; 4], 5, 3, false, vec![]);
        match result.check_success() {
            Err(UploadFailure::IncompleteConfirmations {
                under_replicated,
                fully_confirmed_chunks,
                expected_chunks,
            }) => {
                assert_eq!(under_replicated, vec![0, 1, 2, 3]);
                assert_eq!(fully_confirmed_chunks, 0);
                assert_eq!(expected_chunks, 4);
            }
            other => panic!("expected IncompleteConfirmations, got {other:?}"),
        }
    }

    /// R=1, N=5 → per-chunk plan target = 1. That one ACK per chunk
    /// → success (boundary case for R<N).
    #[test]
    fn upload_r_1_n_5_one_ack_succeeds() {
        let result = make_result_effective(vec![1; 4], 1, 5, false, vec![]);
        assert!(result.check_success().is_ok());
    }

    /// R=0, N=5 → `NoEligibleTargets` with **truthful**
    /// `configured_r = 0` and `eligible_snapshot_len = 5` — reviewer
    /// item #2. Values come from the fields, not from the empty
    /// per-chunk vector.
    #[test]
    fn upload_r_0_n_5_returns_truthful_no_eligible_targets() {
        let result = make_result_effective(vec![0; 4], 0, 5, false, vec![]);
        match result.check_success() {
            Err(UploadFailure::NoEligibleTargets {
                configured_r,
                eligible_snapshot_len,
            }) => {
                assert_eq!(configured_r, 0);
                assert_eq!(eligible_snapshot_len, 5);
            }
            other => panic!("expected NoEligibleTargets, got {other:?}"),
        }
    }

    /// R>0, N=0 → `NoEligibleTargets` with **truthful**
    /// `configured_r = 3` and `eligible_snapshot_len = 0`. Both
    /// R=0 and N=0 cases must produce nonzero CLI failure with
    /// distinguishable values (reviewer item #3).
    #[test]
    fn upload_r_3_n_0_returns_truthful_no_eligible_targets() {
        let result = make_result_effective(vec![0; 4], 3, 0, false, vec![]);
        match result.check_success() {
            Err(UploadFailure::NoEligibleTargets {
                configured_r,
                eligible_snapshot_len,
            }) => {
                assert_eq!(configured_r, 3);
                assert_eq!(eligible_snapshot_len, 0);
            }
            other => panic!("expected NoEligibleTargets, got {other:?}"),
        }
    }

    // ── Active-node routing (#37) ─────────────────────────────────────────

    fn orchestrator(url: String) -> UploadOrchestrator {
        UploadOrchestrator::new(Arc::new(L1RpcClient::new(url)), Duration::from_secs(1), 3)
    }

    /// The finalized height is read once and the active-node snapshot is
    /// pinned to exactly that height.
    #[tokio::test]
    async fn active_nodes_reads_finalized_height_once() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([
            (
                "chain_getBlockHeight",
                MockResponse::Result(serde_json::json!({"height": 321, "finality": "finalized"})),
            ),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::Result(serde_json::json!([])),
            ),
        ]))
        .await;
        let orch = orchestrator(server.url());
        orch.active_nodes_at_finalized_head()
            .await
            .expect("snapshot must decode");
        assert_eq!(server.method_count("chain_getBlockHeight"), 1);
        assert_eq!(
            server.first_params("storage_getActiveNodesAtHeight"),
            Some(serde_json::json!([321])),
        );
    }

    /// (e) A finalized-height lookup failure propagates as the upload
    /// operation's typed error (`run` forwards it with `?`); the at-height
    /// snapshot is never queried.
    #[tokio::test]
    async fn active_nodes_propagates_height_failure() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([(
            "chain_getBlockHeight",
            MockResponse::error("finalized height unavailable"),
        )]))
        .await;
        let orch = orchestrator(server.url());
        let res = orch.active_nodes_at_finalized_head().await;
        assert!(res.is_err(), "height failure must propagate");
        assert_eq!(
            server.method_count("storage_getActiveNodesAtHeight"),
            0,
            "no at-height query once the height read failed"
        );
    }
}
