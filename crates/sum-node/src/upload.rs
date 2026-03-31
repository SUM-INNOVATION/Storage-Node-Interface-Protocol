//! Upload orchestrator — pushes chunks to R assigned nodes with confirmation.
//!
//! Instead of pushing to a single peer and relying on MarketSync for
//! replication, the UploadOrchestrator pushes each chunk directly to its
//! R=3 assigned nodes in parallel and waits for confirmation via gossipsub
//! ChunkAnnouncements or ShardResponse ACKs.
//!
//! This eliminates the single-point-of-failure window between upload and
//! replication that exists in the current `ingest` flow.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::sync::RwLock;
use tracing::{info, warn};

use sum_net::{SumNet, SumNetEvent, PeerId};
use sum_net::identity;
use sum_store::{
    SumStore, compute_chunk_assignment, nodes_for_chunk,
    decode_announcement,
};
use sum_types::storage::{DataManifest, REPLICATION_FACTOR};

use crate::rpc_client::L1RpcClient;

// ── Public Types ─────────────────────────────────────────────────────────────

/// Orchestrates uploading a file's chunks to R assigned nodes.
pub struct UploadOrchestrator {
    rpc: Arc<L1RpcClient>,
    timeout: Duration,
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
        Self { rpc, timeout }
    }

    /// Push all chunks in the manifest to their assigned nodes.
    ///
    /// Returns when all pushes are confirmed (ACK responses received)
    /// or when the timeout is reached.
    pub async fn run(
        &self,
        net: &SumNet,
        store: &SumStore,
        manifest: &DataManifest,
        peer_addresses: &HashMap<PeerId, [u8; 20]>,
    ) -> Result<UploadResult> {
        // 1. Get active nodes from L1 and compute assignment
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

        let chunk_count = manifest.chunk_count as u64;
        let assignment = compute_chunk_assignment(
            &manifest.merkle_root,
            chunk_count,
            &node_addrs,
            REPLICATION_FACTOR,
        );

        // 2. Build reverse map: L1 address -> PeerId
        let mut addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();
        for (&pid, &addr) in peer_addresses.iter() {
            addr_to_peer.insert(addr, pid);
        }

        // 3. Push each chunk to its assigned nodes
        let mut total_pushes: u32 = 0;
        let mut confirmed: u32 = 0;
        let mut failed: Vec<UploadFailure> = Vec::new();

        // Track which (cid, peer) pairs we're waiting for confirmation on
        let mut pending_acks: HashSet<(String, PeerId)> = HashSet::new();

        for chunk in &manifest.chunks {
            let assigned = match nodes_for_chunk(&assignment, chunk.chunk_index) {
                Some(a) => a,
                None => continue,
            };

            // Read chunk data from local store
            let data = store.local.get(&chunk.cid)
                .map_err(|e| anyhow::anyhow!("missing chunk {}: {e}", chunk.cid))?;

            for node_addr in assigned {
                let Some(&peer_id) = addr_to_peer.get(node_addr) else {
                    // No known PeerId for this node — try next
                    failed.push(UploadFailure {
                        chunk_index: chunk.chunk_index,
                        cid: chunk.cid.clone(),
                        error: format!("no PeerId for node {}", hex::encode(node_addr)),
                    });
                    total_pushes += 1;
                    continue;
                };

                match net.push_chunk(peer_id, chunk.cid.clone(), data.clone()).await {
                    Ok(()) => {
                        pending_acks.insert((chunk.cid.clone(), peer_id));
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
        }

        info!(
            total_pushes,
            pending_acks = pending_acks.len(),
            "all push requests sent — waiting for confirmations"
        );

        // 4. Wait for ACK responses (ShardReceived with no error)
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut timed_out = false;

        while !pending_acks.is_empty() {
            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                            if response.error.is_none() {
                                // ACK — chunk was accepted
                                if pending_acks.remove(&(response.cid.clone(), peer_id)) {
                                    confirmed += 1;
                                    info!(
                                        cid = %response.cid,
                                        %peer_id,
                                        confirmed,
                                        remaining = pending_acks.len(),
                                        "push confirmed"
                                    );
                                }
                            } else if let Some(ref err) = response.error {
                                // Push was rejected
                                if pending_acks.remove(&(response.cid.clone(), peer_id)) {
                                    warn!(
                                        cid = %response.cid,
                                        %peer_id,
                                        %err,
                                        "push rejected by peer"
                                    );
                                    failed.push(UploadFailure {
                                        chunk_index: 0, // We don't easily know the index here
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
                        unconfirmed = pending_acks.len(),
                        "upload timeout — not all pushes confirmed"
                    );
                    timed_out = true;
                    break;
                }
            }
        }

        // Record remaining pending as failures
        for (cid, peer_id) in &pending_acks {
            failed.push(UploadFailure {
                chunk_index: 0,
                cid: cid.clone(),
                error: format!("timeout — no ACK from {peer_id}"),
            });
        }

        Ok(UploadResult {
            confirmed,
            total: total_pushes,
            timeout: timed_out,
            failed,
        })
    }
}
