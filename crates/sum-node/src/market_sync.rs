//! Market sync worker — automatically discovers and fetches assigned chunks.
//!
//! Polls the L1 for funded files and active nodes, computes the deterministic
//! chunk assignment, and fetches any missing chunks from peers that hold them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use sum_net::{SumNet, PeerId};
use sum_net::identity;
use sum_store::{SumStore, compute_chunk_assignment, chunks_for_node, nodes_for_chunk};
use sum_store::gc::GarbageCollector;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;
use sum_types::rpc_types::StorageFileInfo;
use sum_types::storage::{CHUNK_SIZE, REPLICATION_FACTOR};

use crate::rpc_client::L1RpcClient;

/// Background worker that syncs this node's assigned chunks from the network,
/// and garbage-collects unassigned chunks after a grace period.
pub struct MarketSyncWorker {
    rpc: Arc<L1RpcClient>,
    /// This node's L1 address (20 bytes).
    l1_address: [u8; 20],
    /// This node's L1 address in base58 (for logging).
    l1_address_base58: String,
    /// How often to poll the L1 for assignment changes.
    poll_interval: Duration,
    /// CIDs with in-flight fetch requests (avoid duplicates).
    pending_fetches: HashSet<String>,
    /// When pending_fetches was last cleared (stale entry cleanup).
    last_fetches_cleared: Instant,
    /// Garbage collector for unassigned chunks.
    gc: GarbageCollector,
    /// When the L1 was last successfully polled (for GC safety).
    last_l1_poll: Instant,
    /// Consecutive RPC failures (for exponential backoff).
    consecutive_failures: u32,
}

impl MarketSyncWorker {
    pub fn new(
        rpc: Arc<L1RpcClient>,
        l1_address: [u8; 20],
        l1_address_base58: String,
        poll_interval: Duration,
        gc_grace_period: Duration,
    ) -> Self {
        Self {
            rpc,
            l1_address,
            l1_address_base58,
            poll_interval,
            pending_fetches: HashSet::new(),
            last_fetches_cleared: Instant::now(),
            gc: GarbageCollector::new(gc_grace_period),
            last_l1_poll: Instant::now(),
            consecutive_failures: 0,
        }
    }

    /// Run the market sync loop until shutdown is signalled.
    pub async fn run(
        mut self,
        store: Arc<RwLock<SumStore>>,
        net: Arc<SumNet>,
        peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        info!(
            address = %self.l1_address_base58,
            interval_secs = self.poll_interval.as_secs(),
            "MarketSync worker started"
        );

        let mut interval = tokio::time::interval(self.poll_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Clear stale pending fetches every 5 minutes.
                    if self.last_fetches_cleared.elapsed() > Duration::from_secs(300) {
                        self.pending_fetches.clear();
                        self.last_fetches_cleared = Instant::now();
                    }

                    match self.sync_cycle(&store, &net, &peer_addresses).await {
                        Ok(()) => { self.consecutive_failures = 0; }
                        Err(e) => {
                            self.consecutive_failures += 1;
                            let backoff_secs = self.poll_interval.as_secs()
                                * 2u64.pow(self.consecutive_failures.min(5));
                            warn!(
                                %e,
                                backoff_secs,
                                failures = self.consecutive_failures,
                                "MarketSync cycle failed — backing off"
                            );
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("MarketSync worker shutting down");
                        return;
                    }
                }
            }
        }
    }

    async fn sync_cycle(
        &mut self,
        store: &Arc<RwLock<SumStore>>,
        net: &Arc<SumNet>,
        peer_addresses: &Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> Result<()> {
        // 1. Get funded files and active nodes from L1
        let files = self.rpc.get_funded_files().await?;
        let node_records = self.rpc.get_active_nodes().await?;

        if files.is_empty() || node_records.is_empty() {
            debug!("no funded files or no active nodes — skipping sync");
            return Ok(());
        }

        // 2. Parse and sort node addresses
        let mut node_addrs: Vec<[u8; 20]> = Vec::new();
        for record in &node_records {
            if let Ok(addr) = identity::l1_address_from_base58(&record.address) {
                node_addrs.push(addr);
            }
        }
        node_addrs.sort();

        // Check if we're even in the active node list
        if !node_addrs.contains(&self.l1_address) {
            debug!("this node is not in the active archive node list — skipping sync");
            return Ok(());
        }

        // Build reverse map: L1 address -> PeerId (for fetching)
        let addr_to_peer: HashMap<[u8; 20], PeerId> = {
            let map = peer_addresses.read().await;
            map.iter().map(|(pid, addr)| (*addr, *pid)).collect()
        };

        let mut fetched_count = 0;
        let mut missing_count = 0;

        // 3. For each funded file, compute assignment and fetch missing chunks
        for file in &files {
            if let Err(e) = self.sync_file(
                file, &node_addrs, &addr_to_peer, store, net,
            ).await {
                warn!(merkle_root = %file.merkle_root, %e, "failed to sync file");
            }
        }

        // 4. Run garbage collection
        self.last_l1_poll = Instant::now();
        let assigned_cids = self.compute_assigned_cids(&files, &node_addrs, store).await;
        {
            let store_read = store.read().await;
            match self.gc.mark_and_sweep(&store_read.local, &assigned_cids, self.last_l1_poll) {
                Ok(result) if result.chunks_deleted > 0 => {
                    info!(
                        deleted = result.chunks_deleted,
                        freed_bytes = result.bytes_freed,
                        "GC completed after sync cycle"
                    );
                }
                Err(e) => warn!(%e, "GC failed"),
                _ => {}
            }
        }

        Ok(())
    }

    async fn sync_file(
        &mut self,
        file: &StorageFileInfo,
        node_addrs: &[[u8; 20]],
        addr_to_peer: &HashMap<[u8; 20], PeerId>,
        store: &Arc<RwLock<SumStore>>,
        net: &Arc<SumNet>,
    ) -> Result<()> {
        // Parse merkle_root
        let root_hex = file.merkle_root.strip_prefix("0x").unwrap_or(&file.merkle_root);
        let root_bytes = hex_to_32(root_hex)
            .ok_or_else(|| anyhow::anyhow!("invalid merkle_root hex"))?;

        // Compute chunk count
        let chunk_count = (file.total_size_bytes + CHUNK_SIZE - 1) / CHUNK_SIZE;
        if chunk_count == 0 {
            return Ok(());
        }

        // Compute assignment
        let assignment = compute_chunk_assignment(
            &root_bytes, chunk_count, node_addrs, REPLICATION_FACTOR,
        );

        // Which chunks is THIS node assigned?
        let my_chunks = chunks_for_node(&assignment, &self.l1_address);
        if my_chunks.is_empty() {
            return Ok(()); // Not assigned to this file
        }

        // Check if we have the manifest for this file
        let store_read = store.read().await;
        let has_manifest = store_read.manifest_idx.get_by_merkle_root(&root_bytes).is_some();

        if !has_manifest {
            drop(store_read); // Release lock before network call

            // Find a peer that is assigned to this file and request the manifest
            let assigned_chunk0 = nodes_for_chunk(&assignment, 0);
            if let Some(nodes) = assigned_chunk0 {
                for node_addr in nodes {
                    if let Some(peer_id) = addr_to_peer.get(node_addr) {
                        info!(
                            root = root_hex,
                            peer = %peer_id,
                            "requesting manifest from peer"
                        );
                        let _ = net.request_manifest(*peer_id, root_hex.to_string()).await;
                        // The manifest will arrive asynchronously via ShardReceived event.
                        // We'll pick up the chunks in the next sync cycle.
                        return Ok(());
                    }
                }
            }
            debug!(root = root_hex, "no peers available to fetch manifest from");
            return Ok(());
        }

        // We have the manifest — check which assigned chunks are missing
        for chunk_index in &my_chunks {
            let cid = store_read.manifest_idx.chunk_cid(&root_bytes, *chunk_index);
            let Some(cid) = cid else { continue };

            if store_read.local.has(cid) {
                continue; // Already have this chunk
            }

            if self.pending_fetches.contains(cid) {
                continue; // Already fetching
            }

            // Find a peer to fetch from
            let chunk_holders = nodes_for_chunk(&assignment, *chunk_index);
            if let Some(holders) = chunk_holders {
                for holder_addr in holders {
                    if holder_addr == &self.l1_address {
                        continue; // Don't fetch from ourselves
                    }
                    if let Some(peer_id) = addr_to_peer.get(holder_addr) {
                        info!(
                            root = root_hex,
                            chunk = chunk_index,
                            cid = cid,
                            peer = %peer_id,
                            "fetching assigned chunk from peer"
                        );
                        let cid_owned = cid.to_string();
                        let _ = net.request_shard_chunk(
                            *peer_id, cid_owned.clone(), None, None,
                        ).await;
                        self.pending_fetches.insert(cid_owned);
                        break; // One fetch request per chunk
                    }
                }
            }
        }

        Ok(())
    }

    /// Compute the complete set of CIDs this node is assigned to across all files.
    async fn compute_assigned_cids(
        &self,
        files: &[StorageFileInfo],
        node_addrs: &[[u8; 20]],
        store: &Arc<RwLock<SumStore>>,
    ) -> HashSet<String> {
        let mut assigned_cids = HashSet::new();
        let store_read = store.read().await;

        for file in files {
            let root_hex = file.merkle_root.strip_prefix("0x").unwrap_or(&file.merkle_root);
            let Some(root_bytes) = hex_to_32(root_hex) else { continue };

            let chunk_count = (file.total_size_bytes + CHUNK_SIZE - 1) / CHUNK_SIZE;
            if chunk_count == 0 { continue; }

            let assignment = compute_chunk_assignment(
                &root_bytes, chunk_count, node_addrs, REPLICATION_FACTOR,
            );

            let my_chunks = chunks_for_node(&assignment, &self.l1_address);

            // Look up CIDs from the manifest if we have it
            if let Some(manifest) = store_read.manifest_idx.get_by_merkle_root(&root_bytes) {
                for chunk_index in &my_chunks {
                    if let Some(chunk) = manifest.chunks.get(*chunk_index as usize) {
                        assigned_cids.insert(chunk.cid.clone());
                    }
                }
            }
        }

        assigned_cids
    }

    /// Called by the main event loop when a chunk fetch completes.
    pub fn on_fetch_complete(&mut self, cid: &str) {
        self.pending_fetches.remove(cid);
    }
}

/// Parse a hex string into [u8; 32]. Returns None if invalid.
fn hex_to_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(bytes)
}
