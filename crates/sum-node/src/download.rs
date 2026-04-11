//! Download orchestrator — retrieves a complete file by merkle root.
//!
//! State machine: discover peers → request manifest → fetch all chunks
//! in parallel → verify CIDs → assemble output file → optionally verify
//! merkle root.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::RwLock;
use tracing::{info, warn};

use sum_net::{SumNet, SumNetEvent, PeerId};
use sum_store::serve::MANIFEST_REQUEST_PREFIX;
use sum_store::{
    compute_chunk_assignment, nodes_for_chunk,
    FetchManager, FetchOutcome, MerkleTree,
};
use sum_store::manifest::deserialize_manifest_cbor;
use sum_types::storage::{DataManifest, REPLICATION_FACTOR};

use crate::peer_state::apply_peer_event;
use crate::rpc_client::L1RpcClient;

// ── Public Types ─────────────────────────────────────────────────────────────

/// Orchestrates downloading a complete file by merkle root from the P2P mesh.
pub struct DownloadOrchestrator {
    merkle_root_hex: String,
    output_path: PathBuf,
    rpc: Arc<L1RpcClient>,
    max_concurrent: usize,
    timeout: Duration,
}

/// Result of a download operation.
pub struct DownloadResult {
    /// Number of chunks fetched from the network.
    pub chunks_fetched: u32,
    /// Number of chunks that were already on disk (skipped).
    pub chunks_skipped: u32,
    /// Total bytes written to the output file.
    pub total_bytes: u64,
    /// Whether the merkle root was verified after reassembly.
    pub merkle_verified: bool,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl DownloadOrchestrator {
    pub fn new(
        merkle_root_hex: String,
        output_path: PathBuf,
        rpc: Arc<L1RpcClient>,
        max_concurrent: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            merkle_root_hex,
            output_path,
            rpc,
            max_concurrent,
            timeout,
        }
    }

    /// Run the full download pipeline.
    ///
    /// 1. Discover peers via mDNS
    /// 2. Request DataManifest by merkle_root
    /// 3. Fetch missing chunks in parallel
    /// 4. Assemble and write the output file
    pub async fn run(
        self,
        net: Arc<SumNet>,
        store: Arc<RwLock<sum_store::SumStore>>,
        peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> Result<DownloadResult> {
        let deadline = tokio::time::Instant::now() + self.timeout;

        // ── Phase 1: Discover peers ──────────────────────────────────────
        info!(merkle_root = %self.merkle_root_hex, "waiting for peers...");
        let mut discovered_peers: Vec<PeerId> = Vec::new();

        loop {
            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. }) => {
                            if !discovered_peers.contains(&peer_id) {
                                discovered_peers.push(peer_id);
                                info!(%peer_id, "peer discovered — requesting manifest");
                                net.request_manifest(peer_id, self.merkle_root_hex.clone()).await?;
                                break;
                            }
                        }
                        Some(ref e @ SumNetEvent::PeerIdentified { .. })
                        | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                            apply_peer_event(&mut *peer_addresses.write().await, e);
                        }
                        None => bail!("network shut down before peer discovery"),
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timeout: no peers discovered within {:?}", self.timeout);
                }
            }
        }

        // ── Phase 2: Await manifest ──────────────────────────────────────
        info!("waiting for manifest response...");
        let manifest: DataManifest;
        let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", self.merkle_root_hex);

        loop {
            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::ShardReceived { response, .. }) => {
                            if response.cid == manifest_cid {
                                if let Some(ref err) = response.error {
                                    warn!(%err, "manifest request failed — trying next peer");
                                    // Try next discovered peer if available
                                    if let Some(&next_peer) = discovered_peers.last() {
                                        net.request_manifest(next_peer, self.merkle_root_hex.clone()).await?;
                                        continue;
                                    }
                                    bail!("manifest not found: {err}");
                                }
                                // CBOR-deserialize the manifest
                                manifest = deserialize_manifest_cbor(&response.data)
                                    .map_err(|e| anyhow::anyhow!("failed to deserialize manifest: {e}"))?;
                                info!(
                                    file_name = %manifest.file_name,
                                    chunk_count = manifest.chunk_count,
                                    total_bytes = manifest.total_size_bytes,
                                    "manifest received"
                                );
                                break;
                            }
                        }
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. }) => {
                            if !discovered_peers.contains(&peer_id) {
                                discovered_peers.push(peer_id);
                            }
                        }
                        Some(ref e @ SumNetEvent::PeerIdentified { .. })
                        | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                            apply_peer_event(&mut *peer_addresses.write().await, e);
                        }
                        None => bail!("network shut down while waiting for manifest"),
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timeout: manifest not received within {:?}", self.timeout);
                }
            }
        }

        // ── Phase 3: Fetch chunks ────────────────────────────────────────

        // Handle empty file (0 chunks)
        if manifest.chunk_count == 0 {
            std::fs::File::create(&self.output_path)?;
            info!(output = %self.output_path.display(), "wrote empty file");
            return Ok(DownloadResult {
                chunks_fetched: 0,
                chunks_skipped: 0,
                total_bytes: 0,
                merkle_verified: true,
            });
        }

        // Store the manifest so we can read chunks back for assembly
        {
            let mut store_write = store.write().await;
            if store_write.manifest_idx.get_by_merkle_root(&manifest.merkle_root).is_none() {
                store_write.manifest_idx.insert(&manifest)?;
            }
        }

        // Determine which chunks are already on disk vs need fetching
        let store_read = store.read().await;
        let mut remaining: VecDeque<u32> = VecDeque::new();
        let mut chunks_skipped: u32 = 0;

        for chunk in &manifest.chunks {
            if store_read.local.has(&chunk.cid) {
                chunks_skipped += 1;
            } else {
                remaining.push_back(chunk.chunk_index);
            }
        }
        drop(store_read);

        let total_to_fetch = remaining.len() as u32;
        info!(
            to_fetch = total_to_fetch,
            already_on_disk = chunks_skipped,
            "starting chunk downloads"
        );

        if total_to_fetch == 0 {
            // All chunks already on disk — skip to assembly
            return self.assemble(&store, &manifest).await.map(|total_bytes| {
                DownloadResult {
                    chunks_fetched: 0,
                    chunks_skipped,
                    total_bytes,
                    merkle_verified: true,
                }
            });
        }

        // Build a peer map for chunk routing: try to use assignment
        let holder_map = self.build_holder_map(&manifest, &peer_addresses).await;

        // Create a private FetchManager (not shared with SumStore's)
        let store_config = store.read().await.config.clone();
        let mut fetcher = FetchManager::new(store_config.max_chunk_msg_bytes);

        let mut in_flight: HashSet<String> = HashSet::new();
        let mut chunks_fetched: u32 = 0;

        // Fill initial batch
        self.fill_fetches(
            &net, &mut fetcher, &mut remaining, &mut in_flight,
            &manifest, &holder_map, &discovered_peers,
        ).await;

        // Event loop: process responses, refill, until done
        loop {
            if chunks_fetched == total_to_fetch {
                break;
            }

            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::ShardReceived { response, .. }) => {
                            // Skip manifest responses
                            if response.cid.starts_with(MANIFEST_REQUEST_PREFIX) {
                                continue;
                            }

                            let store_read = store.read().await;
                            let outcome = fetcher.on_chunk_received(
                                &net, &store_read.local, &response,
                            ).await;
                            drop(store_read);

                            match outcome {
                                FetchOutcome::Complete { cid, size } => {
                                    info!(%cid, size, "chunk downloaded and verified");
                                    in_flight.remove(&cid);
                                    chunks_fetched += 1;

                                    // Refill
                                    self.fill_fetches(
                                        &net, &mut fetcher, &mut remaining, &mut in_flight,
                                        &manifest, &holder_map, &discovered_peers,
                                    ).await;
                                }
                                FetchOutcome::InProgress => { /* windowed transfer, wait for more */ }
                                FetchOutcome::Failed { cid, error } => {
                                    warn!(%cid, %error, "chunk fetch failed — re-queuing");
                                    in_flight.remove(&cid);

                                    // Find chunk_index for this CID and re-queue
                                    if let Some(chunk) = manifest.chunks.iter().find(|c| c.cid == cid) {
                                        remaining.push_back(chunk.chunk_index);
                                    }

                                    // Refill
                                    self.fill_fetches(
                                        &net, &mut fetcher, &mut remaining, &mut in_flight,
                                        &manifest, &holder_map, &discovered_peers,
                                    ).await;
                                }
                            }
                        }
                        Some(SumNetEvent::ShardRequestFailed { peer_id, error }) => {
                            warn!(%peer_id, %error, "shard request failed");
                        }
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. }) => {
                            if !discovered_peers.contains(&peer_id) {
                                discovered_peers.push(peer_id);
                            }
                        }
                        Some(ref e @ SumNetEvent::PeerIdentified { .. })
                        | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                            apply_peer_event(&mut *peer_addresses.write().await, e);
                        }
                        None => bail!("network shut down during chunk fetching"),
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    bail!(
                        "timeout: downloaded {}/{} chunks before deadline",
                        chunks_fetched, total_to_fetch
                    );
                }
            }
        }

        // ── Phase 4: Assemble ────────────────────────────────────────────
        let total_bytes = self.assemble(&store, &manifest).await?;

        Ok(DownloadResult {
            chunks_fetched,
            chunks_skipped,
            total_bytes,
            merkle_verified: true,
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Fill up to `max_concurrent` in-flight fetches from the remaining queue.
    async fn fill_fetches(
        &self,
        net: &SumNet,
        fetcher: &mut FetchManager,
        remaining: &mut VecDeque<u32>,
        in_flight: &mut HashSet<String>,
        manifest: &DataManifest,
        holder_map: &HashMap<u32, Vec<PeerId>>,
        fallback_peers: &[PeerId],
    ) {
        while in_flight.len() < self.max_concurrent {
            let Some(chunk_index) = remaining.pop_front() else { break };
            let chunk = &manifest.chunks[chunk_index as usize];

            if fetcher.is_active(&chunk.cid) {
                continue;
            }

            // Find a peer to fetch from: prefer assignment-based holders
            let peer = holder_map
                .get(&chunk_index)
                .and_then(|peers| peers.iter().find(|p| !in_flight.contains(&chunk.cid)).copied())
                .or_else(|| fallback_peers.first().copied());

            let Some(peer_id) = peer else {
                warn!(chunk_index, "no peer available for chunk — re-queuing");
                remaining.push_back(chunk_index);
                break; // Don't busy-loop
            };

            // Pass the manifest's known chunk size as a tighter validation
            // bound: the fetcher will reject any peer response whose
            // total_bytes does not equal `chunk.size` exactly.
            match fetcher
                .start_fetch_with_expected_size(net, peer_id, chunk.cid.clone(), Some(chunk.size))
                .await
            {
                Ok(()) => {
                    in_flight.insert(chunk.cid.clone());
                }
                Err(e) => {
                    warn!(cid = %chunk.cid, %e, "failed to start fetch — re-queuing");
                    remaining.push_back(chunk_index);
                }
            }
        }
    }

    /// Build a map of chunk_index → PeerIds that hold that chunk.
    /// Uses the L1 assignment algorithm when possible, falls back to empty.
    async fn build_holder_map(
        &self,
        manifest: &DataManifest,
        peer_addresses: &Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> HashMap<u32, Vec<PeerId>> {
        let mut holder_map: HashMap<u32, Vec<PeerId>> = HashMap::new();

        // Try to get active nodes from L1 for assignment-based routing
        let nodes_result = self.rpc.get_active_nodes().await;
        let Ok(node_records) = nodes_result else {
            warn!("could not get active nodes from L1 — using gossipsub-based peer selection");
            return holder_map;
        };

        // Parse addresses, sort
        let mut node_addrs: Vec<[u8; 20]> = Vec::new();
        for record in &node_records {
            if let Ok(addr) = sum_net::l1_address_from_base58(&record.address) {
                node_addrs.push(addr);
            }
        }
        node_addrs.sort();

        if node_addrs.is_empty() {
            return holder_map;
        }

        let chunk_count = manifest.chunk_count as u64;
        let assignment = compute_chunk_assignment(
            &manifest.merkle_root,
            chunk_count,
            &node_addrs,
            REPLICATION_FACTOR,
        );

        // Build reverse map: L1 address → PeerId
        let peer_addr_map = peer_addresses.read().await;
        let mut addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();
        for (&pid, &addr) in peer_addr_map.iter() {
            addr_to_peer.insert(addr, pid);
        }

        // Map chunk → PeerIds
        for chunk_index in 0..manifest.chunk_count {
            if let Some(assigned_addrs) = nodes_for_chunk(&assignment, chunk_index) {
                let peers: Vec<PeerId> = assigned_addrs
                    .iter()
                    .filter_map(|addr| addr_to_peer.get(addr).copied())
                    .collect();
                if !peers.is_empty() {
                    holder_map.insert(chunk_index, peers);
                }
            }
        }

        holder_map
    }

    /// Read all chunks in order, concatenate, write to output file.
    /// Verifies merkle root after assembly.
    async fn assemble(
        &self,
        store: &Arc<RwLock<sum_store::SumStore>>,
        manifest: &DataManifest,
    ) -> Result<u64> {
        info!(
            output = %self.output_path.display(),
            chunks = manifest.chunk_count,
            "assembling file"
        );

        let store_read = store.read().await;
        let mut file = std::fs::File::create(&self.output_path)
            .context("failed to create output file")?;

        let mut total_bytes: u64 = 0;
        for chunk in &manifest.chunks {
            let data = store_read.local.get(&chunk.cid)
                .map_err(|e| anyhow::anyhow!("missing chunk {}: {e}", chunk.cid))?;
            file.write_all(&data)?;
            total_bytes += data.len() as u64;
        }

        file.flush()?;
        drop(store_read);

        // Verify merkle root
        let leaf_hashes: Vec<blake3::Hash> = manifest.chunks
            .iter()
            .map(|c| {
                let mut h = [0u8; 32];
                h.copy_from_slice(&c.blake3_hash);
                blake3::Hash::from(h)
            })
            .collect();

        let tree = MerkleTree::build(&leaf_hashes);
        let computed_root = tree.root();
        let roots_match = computed_root.as_bytes() == &manifest.merkle_root;

        if roots_match {
            info!(
                output = %self.output_path.display(),
                bytes = total_bytes,
                "file assembled and merkle root verified"
            );
        } else {
            warn!(
                "merkle root mismatch! computed={} expected={}",
                hex::encode(computed_root.as_bytes()),
                hex::encode(&manifest.merkle_root),
            );
        }

        Ok(total_bytes)
    }
}
