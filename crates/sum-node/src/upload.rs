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

use anyhow::{bail, Result};
use async_trait::async_trait;
use tracing::{info, warn};

use sum_net::{SumNet, SumNetEvent, PeerId};
use sum_net::identity;
use sum_store::{
    SumStore, compute_chunk_assignment, nodes_for_chunk,
};
use sum_types::storage::{DataManifest, REPLICATION_FACTOR};

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
    async fn push_chunk_shared(
        &self,
        peer_id: PeerId,
        cid: String,
        data: Arc<[u8]>,
    ) -> Result<()>;

    /// Pull the next network event from the swarm. Returns `None` when the
    /// underlying network has shut down.
    async fn next_event(&self) -> Option<SumNetEvent>;
}

#[async_trait]
impl UploadNet for SumNet {
    async fn push_chunk_shared(
        &self,
        peer_id: PeerId,
        cid: String,
        data: Arc<[u8]>,
    ) -> Result<()> {
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
/// `DEFAULT_MAX_IN_FLIGHT_CHUNKS * REPLICATION_FACTOR = 12`.
pub const DEFAULT_MAX_IN_FLIGHT_CHUNKS: usize = 4;

/// Orchestrates uploading a file's chunks to R assigned nodes.
pub struct UploadOrchestrator {
    rpc: Arc<L1RpcClient>,
    timeout: Duration,
    max_in_flight_chunks: usize,
}

/// Result of an upload operation.
pub struct UploadResult {
    /// Number of (chunk, node) push confirmations received.
    pub confirmed: u32,
    /// Total (chunk, node) pushes attempted.
    pub total: u32,
    /// Whether the timeout was reached before all confirmations.
    pub timeout: bool,
    /// Details of failed pushes.
    pub failed: Vec<UploadFailure>,
}

/// A single failed push attempt.
pub struct UploadFailure {
    pub chunk_index: u32,
    pub cid: String,
    pub error: String,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl UploadOrchestrator {
    pub fn new(rpc: Arc<L1RpcClient>, timeout: Duration) -> Self {
        Self {
            rpc,
            timeout,
            max_in_flight_chunks: DEFAULT_MAX_IN_FLIGHT_CHUNKS,
        }
    }

    /// Override the default `max_in_flight_chunks` cap.
    /// Values < 1 are clamped to 1.
    pub fn with_max_in_flight_chunks(mut self, n: usize) -> Self {
        self.max_in_flight_chunks = n.max(1);
        self
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
        // Fetch active node directory from L1.
        let node_records = self.rpc.get_active_nodes().await?;
        let mut node_addrs: Vec<[u8; 20]> = Vec::new();
        for record in &node_records {
            if let Ok(addr) = identity::l1_address_from_base58(&record.address) {
                node_addrs.push(addr);
            }
        }
        node_addrs.sort();

        if node_addrs.is_empty() {
            bail!("no active nodes on L1 — cannot upload");
        }

        self.run_with_nodes(net, store, manifest, peer_addresses, &node_addrs).await
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
        let assignment = compute_chunk_assignment(
            &manifest.merkle_root,
            chunk_count,
            node_addrs,
            REPLICATION_FACTOR,
        );

        // Reverse map: L1 address -> PeerId
        let mut addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();
        for (&pid, &addr) in peer_addresses.iter() {
            addr_to_peer.insert(addr, pid);
        }

        let mut total_pushes: u32 = 0;
        let mut confirmed: u32 = 0;
        let mut failed: Vec<UploadFailure> = Vec::new();
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
                let raw = store.local.get(&chunk.cid)
                    .map_err(|e| anyhow::anyhow!("missing chunk {}: {e}", chunk.cid))?;
                let data: Arc<[u8]> = Arc::from(raw.into_boxed_slice());

                for node_addr in assigned {
                    let Some(&peer_id) = addr_to_peer.get(node_addr) else {
                        failed.push(UploadFailure {
                            chunk_index: chunk.chunk_index,
                            cid: chunk.cid.clone(),
                            error: format!("no PeerId for node {}", hex::encode(node_addr)),
                        });
                        total_pushes += 1;
                        continue;
                    };

                    // Cheap pointer-bump clone — all R replicas share the
                    // same backing buffer.
                    match net.push_chunk_shared(peer_id, chunk.cid.clone(), Arc::clone(&data)).await {
                        Ok(()) => {
                            slice_pending.insert((chunk.cid.clone(), peer_id));
                            total_pushes += 1;
                        }
                        Err(e) => {
                            failed.push(UploadFailure {
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
                                        failed.push(UploadFailure {
                                            chunk_index: 0,
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
                            failed.push(UploadFailure {
                                chunk_index: 0,
                                cid,
                                error: format!("timeout — no ACK from {peer_id}"),
                            });
                        }
                        break 'outer;
                    }
                }
            }
        }

        Ok(UploadResult {
            confirmed,
            total: total_pushes,
            timeout: timed_out,
            failed,
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
        let orch = UploadOrchestrator::new(rpc, Duration::from_secs(1))
            .with_max_in_flight_chunks(0);
        assert_eq!(orch.max_in_flight_chunks, 1);
    }

    #[test]
    fn default_max_in_flight_is_set() {
        let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
        let orch = UploadOrchestrator::new(rpc, Duration::from_secs(1));
        assert_eq!(orch.max_in_flight_chunks, DEFAULT_MAX_IN_FLIGHT_CHUNKS);
    }
}
