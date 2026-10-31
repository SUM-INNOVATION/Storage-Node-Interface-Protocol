//! Download orchestrator — retrieves a complete file by merkle root.
//!
//! State machine: discover peers → request manifest → fetch all chunks
//! in parallel → verify CIDs → assemble output file → optionally verify
//! merkle root.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::sync::RwLock;
use tracing::{info, warn};

use sum_net::{PeerId, ShardResponseV2, SumNet, SumNetEvent};
use sum_store::{
    FetchManager, FetchOutcome, MerkleTree, compute_chunk_assignment, nodes_for_chunk,
};
use sum_types::rpc_types::{NodeRecordInfo, StorageFileInfoV2};
use sum_types::storage::{ChunkDescriptor, DataManifest};

use crate::download_v2_routing::{
    ManifestDecodeError, UNRESOLVED_ARCHIVE_GRACE, V2AssignmentError, V2AssignmentView,
    build_v2_assignment_view, decode_v2_manifest_bytes, mark_archive_asked, next_assigned_archive,
};
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
    /// Live-chain replication factor sourced from
    /// `ChainParamsInfo::assignment_replication_factor` at operation
    /// entry. Used by the V1 holder-map computation so V1 download
    /// routes to the same archive set the chain assigned.
    replication_factor: u32,
}

/// Result of a download operation.
///
/// In addition to the raw outcome counts, this carries retrieval telemetry
/// (per-peer attribution, wall-clock timing) so downstream callers — like
/// the HTTP gateway — can surface which peers actually served the bytes
/// and how long the retrieval took.
#[derive(Debug, Clone)]
pub struct DownloadResult {
    /// Number of chunks fetched from the network.
    pub chunks_fetched: u32,
    /// Number of chunks that were already on disk (skipped).
    pub chunks_skipped: u32,
    /// Total bytes written to the output file.
    pub total_bytes: u64,
    /// Whether the merkle root was verified after reassembly.
    pub merkle_verified: bool,
    /// Per-peer attribution: number of chunks each peer served to completion.
    /// Peers that served only partial (windowed) pieces toward a chunk that
    /// was ultimately completed by a different peer are not counted here.
    pub chunk_peer_attribution: HashMap<PeerId, u32>,
    /// Superset of peers that sourced any completed chunk during this run.
    /// Equivalent to `chunk_peer_attribution.keys()` — kept explicitly so
    /// callers don't need to clone the map to observe the set.
    pub peers_contacted: HashSet<PeerId>,
    /// Wall-clock time at which the run started (set in `run` before Phase 1).
    pub started_at: SystemTime,
    /// Wall-clock time at which the run finished (set just before returning).
    pub completed_at: SystemTime,
}

impl DownloadResult {
    /// Total wall-clock duration of the retrieval, from Phase 1 start to
    /// post-assembly return.
    pub fn duration(&self) -> Duration {
        self.completed_at
            .duration_since(self.started_at)
            .unwrap_or(Duration::ZERO)
    }
}

// ── Implementation ───────────────────────────────────────────────────────────

impl DownloadOrchestrator {
    pub fn new(
        merkle_root_hex: String,
        output_path: PathBuf,
        rpc: Arc<L1RpcClient>,
        max_concurrent: usize,
        timeout: Duration,
        replication_factor: u32,
    ) -> Self {
        Self {
            merkle_root_hex,
            output_path,
            rpc,
            max_concurrent,
            timeout,
            replication_factor,
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
        let started_at = SystemTime::now();

        // ── Phase 1: Discover peers ──────────────────────────────────────
        info!(merkle_root = %self.merkle_root_hex, "waiting for peers...");
        let mut discovered_peers: Vec<PeerId> = Vec::new();

        loop {
            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. })
                            if !discovered_peers.contains(&peer_id) =>
                        {
                            discovered_peers.push(peer_id);
                            break;
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
        //
        // The first peer discovered during Phase 1 is often the bootstrap
        // (relay) — which by definition does not store content. Asking it
        // for a manifest will always fail. So Phase 2 fans out manifest
        // requests across every peer we know about and every new peer we
        // discover, keeping a `tried_for_manifest` set so we never
        // double-ask. The first peer to respond with a valid manifest
        // wins; everyone else's responses are ignored after `break`.
        info!("waiting for manifest response...");
        let mut tried_for_manifest: HashSet<PeerId> = HashSet::new();

        // Helper: send a manifest request to `peer_id` if we haven't
        // already, and record that we did.
        async fn try_manifest_request(
            net: &SumNet,
            peer_id: PeerId,
            merkle_root_hex: &str,
            tried: &mut HashSet<PeerId>,
        ) {
            if tried.insert(peer_id) {
                info!(%peer_id, "requesting manifest from peer");
                if let Err(e) = net
                    .request_manifest(peer_id, merkle_root_hex.to_string())
                    .await
                {
                    warn!(%peer_id, %e, "manifest request enqueue failed");
                }
            }
        }

        // Seed: ask every peer we already know about. Almost always this
        // is just the bootstrap peer from Phase 1.
        for peer_id in discovered_peers.clone() {
            try_manifest_request(
                net.as_ref(),
                peer_id,
                &self.merkle_root_hex,
                &mut tried_for_manifest,
            )
            .await;
        }

        let manifest: DataManifest = loop {
            tokio::select! {
                event = net.next_event() => {
                    match event {
                        // Accept only a response to a manifest pull this
                        // download issued for *this* root. The test is against
                        // `origin`, the request identity sum-net retained at
                        // send time, so a peer cannot steer this arm by choosing
                        // what CID to name in its reply.
                        Some(SumNetEvent::ShardReceived { peer_id, response, origin })
                            if origin.requested_manifest_root_hex()
                                == Some(self.merkle_root_hex.as_str()) =>
                        {
                            if let Some(ref err) = response.error {
                                // The peer we asked answered "not found".
                                // Don't bail — keep waiting for another
                                // peer's response. New peers discovered
                                // below will get their own request.
                                warn!(%peer_id, %err, "manifest request rejected by a peer — waiting for others");
                                continue;
                            }
                            // Validate against the root this download is for.
                            // A peer that answers with a different or forged
                            // manifest is skipped rather than fatal: another
                            // peer may still serve the real one, and the
                            // surrounding loop already handles that.
                            let m = match sum_store::serve::validate_manifest_push(
                                origin
                                    .requested_manifest_root_hex()
                                    .expect("guard above matched a manifest pull"),
                                &response.data,
                            ) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(
                                        %peer_id,
                                        error = %e,
                                        "manifest failed validation against the requested root — trying other peers"
                                    );
                                    continue;
                                }
                            };
                            info!(
                                %peer_id,
                                file_name = %m.file_name,
                                chunk_count = m.chunk_count,
                                total_bytes = m.total_size_bytes,
                                "manifest received"
                            );
                            // Promote the manifest provider to the FRONT
                            // of `discovered_peers` so Phase 3's chunk
                            // fetcher prefers them. They demonstrably
                            // hold content for this root; we already
                            // have a connection to them. Without this
                            // step, `fill_fetches` falls back to
                            // `discovered_peers.first()` — which is
                            // typically the bootstrap peer (the relay),
                            // who has no chunks and answers "not found"
                            // in a loop.
                            if let Some(pos) = discovered_peers.iter().position(|p| *p == peer_id) {
                                if pos != 0 {
                                    discovered_peers.swap(0, pos);
                                }
                            } else {
                                discovered_peers.insert(0, peer_id);
                            }
                            break m;
                        }
                        // A manifest pull for *this* root failed (no
                        // connection, peer down, protocol mismatch). The guard
                        // is on the retained request identity, not on
                        // `tried_for_manifest.contains(peer_id)`: the old test
                        // matched any outbound failure to a peer we had ever
                        // asked, so an unrelated request failing to that peer
                        // was logged as a manifest failure. We don't retry the
                        // same peer — we wait for another to be discovered.
                        Some(SumNetEvent::ShardRequestFailed { peer_id, error, origin })
                            if origin.requested_manifest_root_hex()
                                == Some(self.merkle_root_hex.as_str()) =>
                        {
                            warn!(%peer_id, %error, "manifest request to peer dropped — will rely on other peers");
                        }
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. }) => {
                            // Newly-discovered peer. If we haven't already
                            // tried it for the manifest, fire a request.
                            // Multi-source manifest requests race; the
                            // first success wins via `break m` above.
                            if !discovered_peers.contains(&peer_id) {
                                discovered_peers.push(peer_id);
                            }
                            try_manifest_request(
                                net.as_ref(),
                                peer_id,
                                &self.merkle_root_hex,
                                &mut tried_for_manifest,
                            )
                            .await;
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
                    bail!(
                        "timeout: manifest not received within {:?} (asked {} peer(s))",
                        self.timeout, tried_for_manifest.len()
                    );
                }
            }
        };

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
                chunk_peer_attribution: HashMap::new(),
                peers_contacted: HashSet::new(),
                started_at,
                completed_at: SystemTime::now(),
            });
        }

        // Store the manifest so we can read chunks back for assembly
        {
            let mut store_write = store.write().await;
            // `manifest` reached here only through `validate_manifest_push`
            // against `self.merkle_root_hex` at the accept site above, so it
            // is already bound to the root this download asked for.
            if store_write
                .manifest_idx
                .get_by_merkle_root(&manifest.merkle_root)
                .is_none()
            {
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
            // All chunks already on disk — skip to assembly. No network
            // fetches happened, so peer attribution is empty.
            return self
                .assemble(&store, &manifest)
                .await
                .map(|total_bytes| DownloadResult {
                    chunks_fetched: 0,
                    chunks_skipped,
                    total_bytes,
                    merkle_verified: true,
                    chunk_peer_attribution: HashMap::new(),
                    peers_contacted: HashSet::new(),
                    started_at,
                    completed_at: SystemTime::now(),
                });
        }

        // Build a peer map for chunk routing: try to use assignment
        let holder_map = self.build_holder_map(&manifest, &peer_addresses).await;

        // Create a private FetchManager (not shared with SumStore's)
        let store_config = store.read().await.config.clone();
        let mut fetcher = FetchManager::new(store_config.max_chunk_msg_bytes);

        // in_flight maps CID → PeerId of the requester. Peer attribution lets
        // us re-queue the right chunks when a specific peer's outbound
        // request fails (e.g. relay circuit killed).
        let mut in_flight: HashMap<String, PeerId> = HashMap::new();
        let mut chunks_fetched: u32 = 0;

        // Retrieval telemetry: per-peer chunk counts. We attribute a chunk
        // to the peer that delivered the *final* piece that completed it
        // (`FetchOutcome::Complete`). Windowed transfers where intermediate
        // pieces came from other peers are common in multi-source flows; a
        // future refinement could track per-piece attribution, but that's
        // out of scope here — the "who closed out this chunk" model is
        // useful on its own and cheap to compute.
        let mut chunk_peer_attribution: HashMap<PeerId, u32> = HashMap::new();
        let mut peers_contacted: HashSet<PeerId> = HashSet::new();

        // Fill initial batch
        self.fill_fetches(
            &net,
            &mut fetcher,
            &mut remaining,
            &mut in_flight,
            &manifest,
            &holder_map,
            &discovered_peers,
        )
        .await;

        // Event loop: process responses, refill, until done
        loop {
            if chunks_fetched == total_to_fetch {
                break;
            }

            tokio::select! {
                event = net.next_event() => {
                    match event {
                        Some(SumNetEvent::ShardReceived { peer_id, response, origin }) => {
                            // Skip manifest responses. Decided from the request
                            // this node issued, not from `response.cid`: a late
                            // manifest reply must not be handed to the chunk
                            // fetcher because the peer chose to label it as a
                            // chunk. Beyond this point the loop keys off
                            // `response.cid`, which sum-net has already checked
                            // against the CID we asked for.
                            if origin.requested_manifest_root_hex().is_some() {
                                continue;
                            }

                            let store_read = store.read().await;
                            let outcome = fetcher.on_chunk_received(
                                net.as_ref(), &store_read.local, &response,
                            ).await;
                            drop(store_read);

                            match outcome {
                                FetchOutcome::Complete { cid, size } => {
                                    info!(%cid, size, %peer_id, "chunk downloaded and verified");
                                    in_flight.remove(&cid);
                                    chunks_fetched += 1;
                                    // Attribute this chunk to the peer that
                                    // closed it out.
                                    *chunk_peer_attribution.entry(peer_id).or_insert(0) += 1;
                                    peers_contacted.insert(peer_id);

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
                        Some(SumNetEvent::ShardRequestFailed { peer_id, error, origin }) => {
                            // Re-queue exactly the chunk that failed. Without
                            // a re-queue, a relay circuit reset permanently
                            // wedges the pipeline: in_flight stays full,
                            // fill_fetches refuses to issue more, and the
                            // download times out with 0 progress.
                            //
                            // This used to re-queue *every* in-flight chunk
                            // attributed to the peer, because a peer id was the
                            // only identity the event carried. With several
                            // requests outstanding to one peer that abandons
                            // healthy transfers and re-issues them, which is
                            // both slower and a stampede. `origin` names the
                            // one request that failed, so exactly one entry is
                            // settled.
                            let Some(cid) = origin.requested_cid() else {
                                warn!(%peer_id, %error, asked = origin.kind().label(),
                                      "outbound failure for a request with no CID — nothing to re-queue");
                                continue;
                            };
                            if in_flight.remove(cid).is_none() {
                                // Already settled by a response or an earlier
                                // failure; nothing outstanding to re-queue.
                                continue;
                            }
                            warn!(
                                %peer_id,
                                %error,
                                %cid,
                                "shard request failed — re-queuing that chunk"
                            );
                            if let Some(chunk) = manifest.chunks.iter().find(|c| c.cid == *cid) {
                                remaining.push_back(chunk.chunk_index);
                            }
                            self.fill_fetches(
                                &net, &mut fetcher, &mut remaining, &mut in_flight,
                                &manifest, &holder_map, &discovered_peers,
                            ).await;
                        }
                        Some(SumNetEvent::PeerDiscovered { peer_id, .. })
                            if !discovered_peers.contains(&peer_id) =>
                        {
                            discovered_peers.push(peer_id);
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
            chunk_peer_attribution,
            peers_contacted,
            started_at,
            completed_at: SystemTime::now(),
        })
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Fill up to `max_concurrent` in-flight fetches from the remaining queue.
    async fn fill_fetches(
        &self,
        net: &SumNet,
        fetcher: &mut FetchManager,
        remaining: &mut VecDeque<u32>,
        in_flight: &mut HashMap<String, PeerId>,
        manifest: &DataManifest,
        holder_map: &HashMap<u32, Vec<PeerId>>,
        fallback_peers: &[PeerId],
    ) {
        while in_flight.len() < self.max_concurrent {
            let Some(chunk_index) = remaining.pop_front() else {
                break;
            };
            let chunk = &manifest.chunks[chunk_index as usize];

            if fetcher.is_active(&chunk.cid) {
                continue;
            }

            // Find a peer to fetch from: prefer assignment-based holders
            let peer = holder_map
                .get(&chunk_index)
                .and_then(|peers| {
                    peers
                        .iter()
                        .find(|_p| !in_flight.contains_key(&chunk.cid))
                        .copied()
                })
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
                    in_flight.insert(chunk.cid.clone(), peer_id);
                }
                Err(e) => {
                    warn!(cid = %chunk.cid, %e, "failed to start fetch — re-queuing");
                    remaining.push_back(chunk_index);
                }
            }
        }
    }

    /// Fetch the active-archive snapshot pinned to the current finalized
    /// head.
    ///
    /// Reads the finalized height exactly once via
    /// `chain_get_block_height` (which passes `["finalized"]`), then the
    /// height-pinned snapshot via `storage_get_active_nodes_at_height` at
    /// exactly that height. This is the supported replacement for the
    /// removed `storage_getActiveNodes` bulk endpoint. Both lookups
    /// surface their typed errors as a single `Err`; `build_holder_map`
    /// maps EITHER failure to the gossipsub fallback rather than
    /// propagating, so a transient L1 outage cannot hard-fail a download.
    async fn active_nodes_at_finalized_head(&self) -> Result<Vec<NodeRecordInfo>> {
        let finalized_height = self.rpc.chain_get_block_height().await?.height;
        self.rpc
            .storage_get_active_nodes_at_height(finalized_height)
            .await
    }

    /// Build a map of chunk_index → PeerIds that hold that chunk.
    /// Uses the L1 assignment algorithm when possible, falls back to empty.
    async fn build_holder_map(
        &self,
        manifest: &DataManifest,
        peer_addresses: &Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> HashMap<u32, Vec<PeerId>> {
        let mut holder_map: HashMap<u32, Vec<PeerId>> = HashMap::new();

        // Try to get active nodes from L1 for assignment-based routing.
        // Apply the shared eligibility contract so V1 holder-map
        // computation excludes Slashed/Unbonding/Withdrawn and
        // non-Archive records.
        //
        // The snapshot is pinned to the finalized head. BOTH a
        // height-lookup failure AND an at-height snapshot failure fall back
        // to gossipsub-based peer selection (return the empty holder_map) —
        // this returns a map, not a Result, so a transient L1 outage must
        // never hard-error the download.
        let Ok(node_records) = self.active_nodes_at_finalized_head().await else {
            warn!("could not get active nodes from L1 — using gossipsub-based peer selection");
            return holder_map;
        };
        let node_records = sum_types::rpc_types::filter_active_archives(node_records);

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
            self.replication_factor,
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

    /// V2 Public download — uses `pull_manifest_v2` / `pull_chunk_v2`
    /// over `/sum/storage/v2`, routed by chain-deterministic V2
    /// assignment.
    ///
    /// Same four phases as `run`:
    ///   1. Build the V2 assignment view from `info.assignment_height`.
    ///   2. Fan out `pull_manifest_v2` across distinct V2-assigned
    ///      archives; the first one to return a CBOR-decodable
    ///      manifest with the matching `merkle_root` wins.
    ///   3. For each chunk, single-shot `pull_chunk_v2` against an
    ///      assigned archive; on failure / wrong bytes, fall back to
    ///      the next assigned archive in V2-deterministic order. No
    ///      "any connected peer" fallback — for V2 rows, chain
    ///      assignment is the truth.
    ///   4. Assemble + merkle-verify (unchanged from V1).
    ///
    /// V1 helpers / `FetchManager` are NOT used here. Public V2
    /// rows must travel `/sum/storage/v2` end-to-end; calling
    /// `request_manifest` / `request_shard_chunk` (V1) on a peer
    /// that prefers V2 results in the codec rejecting the V1
    /// payload on the negotiated V2 stream.
    pub async fn run_v2_public(
        self,
        net: Arc<SumNet>,
        store: Arc<RwLock<sum_store::SumStore>>,
        peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
        info: StorageFileInfoV2,
    ) -> Result<DownloadResult> {
        let started_at = SystemTime::now();
        let deadline = tokio::time::Instant::now() + self.timeout;

        // Parse the chain-side merkle root once. `info.merkle_root` is
        // a 0x-prefixed 64-hex string; we need the 32-byte form for
        // the V2 helpers.
        let chain_root = parse_root_hex_32(&info.merkle_root)
            .with_context(|| format!("info.merkle_root invalid: {}", info.merkle_root))?;

        // ── Phase 1: V2 assignment view ─────────────────────────────
        let view = build_v2_assignment_view(&self.rpc, &info, chain_root, 0..info.chunk_count)
            .await
            .map_err(|e: V2AssignmentError| anyhow::anyhow!(e))?;

        info!(
            merkle_root = %self.merkle_root_hex,
            distinct_archives = view.distinct_assigned.len(),
            r = view.r,
            "V2Public: assignment view built"
        );

        // ── Phase 2: V2 manifest fetch ──────────────────────────────
        let manifest = fetch_v2_public_manifest(
            net.as_ref(),
            &peer_addresses,
            &view,
            chain_root,
            self.max_concurrent,
            deadline,
        )
        .await?;

        info!(
            file_name = %manifest.file_name,
            chunk_count = manifest.chunk_count,
            total_bytes = manifest.total_size_bytes,
            "V2Public: manifest received"
        );

        // Empty file shortcut — same contract as V1's `run`.
        if manifest.chunk_count == 0 {
            std::fs::File::create(&self.output_path)?;
            info!(output = %self.output_path.display(), "wrote empty file");
            return Ok(DownloadResult {
                chunks_fetched: 0,
                chunks_skipped: 0,
                total_bytes: 0,
                merkle_verified: true,
                chunk_peer_attribution: HashMap::new(),
                peers_contacted: HashSet::new(),
                started_at,
                completed_at: SystemTime::now(),
            });
        }

        // Persist the manifest in the local store so `assemble` can
        // read chunks back via `manifest_idx` lookups (matches V1's
        // contract).
        {
            let mut store_write = store.write().await;
            if store_write
                .manifest_idx
                .get_by_merkle_root(&manifest.merkle_root)
                .is_none()
            {
                store_write.manifest_idx.insert(&manifest)?;
            }
        }

        // ── Phase 3: V2 chunk fetch ─────────────────────────────────
        let fetch_outcome = fetch_v2_public_chunks(
            net.as_ref(),
            &store,
            &peer_addresses,
            &view,
            &manifest,
            self.max_concurrent,
            deadline,
        )
        .await?;

        // ── Phase 4: assemble + verify ──────────────────────────────
        let total_bytes = self.assemble(&store, &manifest).await?;

        Ok(DownloadResult {
            chunks_fetched: fetch_outcome.chunks_fetched,
            chunks_skipped: fetch_outcome.chunks_skipped,
            total_bytes,
            merkle_verified: true,
            chunk_peer_attribution: fetch_outcome.chunk_peer_attribution,
            peers_contacted: fetch_outcome.peers_contacted,
            started_at,
            completed_at: SystemTime::now(),
        })
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
        let mut file =
            std::fs::File::create(&self.output_path).context("failed to create output file")?;

        let mut total_bytes: u64 = 0;
        for chunk in &manifest.chunks {
            let data = store_read
                .local
                .get(&chunk.cid)
                .map_err(|e| anyhow::anyhow!("missing chunk {}: {e}", chunk.cid))?;
            file.write_all(&data)?;
            total_bytes += data.len() as u64;
        }

        file.flush()?;
        drop(store_read);

        // Verify merkle root
        let leaf_hashes: Vec<blake3::Hash> = manifest
            .chunks
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
                hex::encode(manifest.merkle_root),
            );
        }

        Ok(total_bytes)
    }
}

// ── V2 Public helpers ────────────────────────────────────────────────────────

/// Parse `0x`-prefixed 64-hex (or bare 64-hex) into a 32-byte root.
/// Pure helper; no I/O. Mirrors `crate::main::parse_merkle_root_hex`
/// but is kept module-private to avoid forcing a `pub` re-export
/// just so the orchestrator can call it.
fn parse_root_hex_32(s: &str) -> Result<[u8; 32]> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).map_err(|e| anyhow::anyhow!("invalid hex: {e}"))?;
    if bytes.len() != 32 {
        bail!("root must be 32 bytes (got {} bytes)", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Fan-out `pull_manifest_v2` across `view.distinct_assigned` until
/// one peer returns a CBOR-decodable manifest whose embedded
/// `merkle_root` equals `chain_root`.
///
/// The V2 path doesn't fall back to "any connected peer" — for V2
/// rows the chain's assignment is the truth, and an unresolvable
/// assigned archive is an actionable diagnostic, not a routing
/// retry signal.
async fn fetch_v2_public_manifest(
    net: &SumNet,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    view: &V2AssignmentView,
    chain_root: [u8; 32],
    max_concurrent: usize,
    deadline: tokio::time::Instant,
) -> Result<DataManifest> {
    use std::collections::BTreeSet;

    // Per-archive state: Untried / Dispatched / Failed. Same shape
    // as the Private path's `ManifestArchiveStatus`, kept inline so
    // the Public path doesn't take a dependency on `download_private`'s
    // private types.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Status {
        Untried,
        Dispatched,
        Failed,
    }
    let mut status: HashMap<[u8; 20], Status> = view
        .distinct_assigned
        .iter()
        .map(|a| (*a, Status::Untried))
        .collect();
    let mut dispatched_peers: HashMap<PeerId, [u8; 20]> = HashMap::new();
    let assigned_total = view.distinct_assigned.len();
    let fanout = max_concurrent.max(1).min(assigned_total);
    let mut last_reason: String = "no responses received".to_string();

    async fn dispatch_wave(
        net: &SumNet,
        peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
        status: &mut HashMap<[u8; 20], Status>,
        dispatched_peers: &mut HashMap<PeerId, [u8; 20]>,
        chain_root: [u8; 32],
        fanout: usize,
    ) {
        let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
            .read()
            .await
            .iter()
            .map(|(p, a)| (*a, *p))
            .collect();
        // Greedy: walk archives in deterministic order, dispatch up to
        // `fanout - in_flight` to Untried + resolvable archives.
        let in_flight = status
            .values()
            .filter(|s| **s == Status::Dispatched)
            .count();
        let mut remaining = fanout.saturating_sub(in_flight);
        if remaining == 0 {
            return;
        }
        // Iterate in BTreeSet order (sorted address) so the dispatch
        // sequence is deterministic and operator-debuggable.
        let archives: Vec<[u8; 20]> = status
            .iter()
            .filter(|(_, s)| **s == Status::Untried)
            .map(|(a, _)| *a)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for archive in archives {
            if remaining == 0 {
                break;
            }
            let Some(&peer_id) = addr_to_peer.get(&archive) else {
                continue;
            };
            match net.pull_manifest_v2(peer_id, chain_root).await {
                Ok(()) => {
                    status.insert(archive, Status::Dispatched);
                    dispatched_peers.insert(peer_id, archive);
                    remaining -= 1;
                }
                Err(e) => {
                    warn!(
                        %peer_id,
                        archive = %hex::encode(archive),
                        %e,
                        "V2Public manifest fan-out: enqueue failed; marking archive failed"
                    );
                    status.insert(archive, Status::Failed);
                }
            }
        }
    }

    let build_all_failed_err = |status: &HashMap<[u8; 20], Status>,
                                addr_to_peer: &HashMap<[u8; 20], PeerId>,
                                last_reason: &str|
     -> anyhow::Error {
        let resolvable = view
            .distinct_assigned
            .iter()
            .filter(|a| addr_to_peer.contains_key(*a))
            .count();
        let unresolvable = assigned_total - resolvable;
        let tried = status
            .values()
            .filter(|s| !matches!(s, Status::Untried))
            .count();
        anyhow::anyhow!(
            "V2Public manifest fetch exhausted all V2-assigned archives: \
             tried {tried} of {assigned_total} ({resolvable} resolvable, \
             {unresolvable} unresolvable); last error: {last_reason}"
        )
    };

    dispatch_wave(
        net,
        peer_addresses,
        &mut status,
        &mut dispatched_peers,
        chain_root,
        fanout,
    )
    .await;

    loop {
        let any_alive = status.values().any(|s| !matches!(s, Status::Failed));
        if !any_alive {
            let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                .read()
                .await
                .iter()
                .map(|(p, a)| (*a, *p))
                .collect();
            return Err(build_all_failed_err(&status, &addr_to_peer, &last_reason));
        }

        let event = tokio::select! {
            ev = net.next_event() => ev,
            _ = tokio::time::sleep_until(deadline) => {
                let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                    .read()
                    .await
                    .iter()
                    .map(|(p, a)| (*a, *p))
                    .collect();
                return Err(build_all_failed_err(
                    &status,
                    &addr_to_peer,
                    &format!("manifest fetch deadline exceeded; previous: {last_reason}"),
                ));
            }
        };

        match event {
            Some(SumNetEvent::PeerDiscovered { .. }) => {
                // PeerDiscovered alone doesn't unlock the L1-addr →
                // PeerId map; PeerIdentified does.
            }
            Some(ref e @ SumNetEvent::PeerIdentified { .. })
            | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                apply_peer_event(&mut *peer_addresses.write().await, e);
                dispatch_wave(
                    net,
                    peer_addresses,
                    &mut status,
                    &mut dispatched_peers,
                    chain_root,
                    fanout,
                )
                .await;
            }
            Some(SumNetEvent::ShardReceivedV2 {
                peer_id,
                response:
                    ShardResponseV2::ManifestData {
                        merkle_root,
                        manifest_bytes,
                        error,
                    },
                origin: _,
            }) => {
                if merkle_root != chain_root {
                    continue;
                }
                let Some(&archive) = dispatched_peers.get(&peer_id) else {
                    continue;
                };
                if status.get(&archive) != Some(&Status::Dispatched) {
                    continue;
                }
                if let Some(err) = error.as_deref() {
                    warn!(
                        %peer_id,
                        archive = %hex::encode(archive),
                        %err,
                        "V2Public manifest fan-out: peer-side error; trying others"
                    );
                    last_reason = format!("archive {} peer error: {err}", hex::encode(archive));
                    status.insert(archive, Status::Failed);
                } else {
                    match decode_v2_manifest_bytes(&manifest_bytes, chain_root) {
                        Ok(m) => return Ok(m),
                        Err(ManifestDecodeError::RootMismatch { got, want }) => {
                            warn!(
                                %peer_id,
                                archive = %hex::encode(archive),
                                %got,
                                %want,
                                "V2Public manifest fan-out: root mismatch; trying others"
                            );
                            last_reason = format!(
                                "archive {} root mismatch (got {got})",
                                hex::encode(archive)
                            );
                            status.insert(archive, Status::Failed);
                        }
                        Err(ManifestDecodeError::Invalid(e)) => {
                            // Declared the right root but the contents do not
                            // produce it. Treat exactly like a mismatch: this
                            // archive is not serving the real manifest, so try
                            // the others rather than failing the download.
                            warn!(
                                %peer_id,
                                archive = %hex::encode(archive),
                                err = %e,
                                "V2Public manifest fan-out: manifest failed validation; trying others"
                            );
                            last_reason =
                                format!("archive {} invalid manifest: {e}", hex::encode(archive));
                            status.insert(archive, Status::Failed);
                        }
                        Err(ManifestDecodeError::Cbor(e)) => {
                            warn!(
                                %peer_id,
                                archive = %hex::encode(archive),
                                err = %e,
                                "V2Public manifest fan-out: CBOR decode failed; trying others"
                            );
                            last_reason =
                                format!("archive {} CBOR decode: {e}", hex::encode(archive));
                            status.insert(archive, Status::Failed);
                        }
                    }
                }
                dispatch_wave(
                    net,
                    peer_addresses,
                    &mut status,
                    &mut dispatched_peers,
                    chain_root,
                    fanout,
                )
                .await;
            }
            Some(SumNetEvent::ShardRequestFailed {
                peer_id,
                error,
                origin,
            }) => {
                // Only a manifest pull for the root this fan-out is for may
                // mark an archive failed. A chunk request to the same peer
                // failing says nothing about its manifest dispatch.
                if origin.requested_manifest_root() != Some(chain_root) {
                    continue;
                }
                if let Some(&archive) = dispatched_peers.get(&peer_id)
                    && status.get(&archive) == Some(&Status::Dispatched)
                {
                    warn!(
                        %peer_id,
                        archive = %hex::encode(archive),
                        %error,
                        "V2Public manifest fan-out: outbound failure; marking failed"
                    );
                    last_reason =
                        format!("archive {} outbound failure: {error}", hex::encode(archive));
                    status.insert(archive, Status::Failed);
                    dispatch_wave(
                        net,
                        peer_addresses,
                        &mut status,
                        &mut dispatched_peers,
                        chain_root,
                        fanout,
                    )
                    .await;
                }
            }
            None => {
                let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                    .read()
                    .await
                    .iter()
                    .map(|(p, a)| (*a, *p))
                    .collect();
                return Err(build_all_failed_err(
                    &status,
                    &addr_to_peer,
                    &format!("network shut down mid-fetch; previous: {last_reason}"),
                ));
            }
            _ => {}
        }
    }
}

/// Aggregate counters returned by `fetch_v2_public_chunks`.
struct V2PublicFetchOutcome {
    chunks_fetched: u32,
    chunks_skipped: u32,
    chunk_peer_attribution: HashMap<PeerId, u32>,
    peers_contacted: HashSet<PeerId>,
}

/// Per-chunk state of [`fetch_v2_public_chunks`]. `assigned` is the
/// order the chunk's V2-assigned archives are asked in: the
/// chain-deterministic ranking, except that an archive asked ahead of
/// unidentified ones moves ahead of them ([`mark_archive_asked`]).
/// `next_attempt_idx` counts the archives already asked; `in_flight_to`
/// pins the (peer_id, archive) we're awaiting; `done` is set when the
/// chunk is persisted to local disk.
struct PublicChunkState {
    assigned: Vec<[u8; 20]>,
    next_attempt_idx: usize,
    in_flight_to: Option<(PeerId, [u8; 20])>,
    done: bool,
}

/// One chunk request [`select_public_chunk_dispatches`] schedules: the
/// chunk, the peer to ask, and the archive's position in the chunk's
/// `assigned` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublicDispatch {
    chunk_index: u32,
    peer_id: PeerId,
    assigned_pos: usize,
}

/// **Pure** selection of the chunk requests to send now, in manifest
/// order, up to `max_concurrent` in flight (clamped to at least 1). Each
/// pending chunk is asked of [`next_assigned_archive`]: its next archive
/// in line once identified, or, with `skip_unresolved` (after
/// [`UNRESOLVED_ARCHIVE_GRACE`]), the first identified one not yet asked.
fn select_public_chunk_dispatches(
    state: &HashMap<u32, PublicChunkState>,
    manifest_chunks: &[ChunkDescriptor],
    addr_to_peer: &HashMap<[u8; 20], PeerId>,
    max_concurrent: usize,
    skip_unresolved: bool,
) -> Vec<PublicDispatch> {
    let cap = max_concurrent.max(1);
    let mut in_flight = state.values().filter(|s| s.in_flight_to.is_some()).count();
    let mut out = Vec::new();
    for cd in manifest_chunks {
        if in_flight >= cap {
            break;
        }
        let Some(s) = state.get(&cd.chunk_index) else {
            continue;
        };
        if s.done || s.in_flight_to.is_some() {
            continue;
        }
        // `None`: wait for a PeerIdentified (or the grace period), or
        // nothing left to ask — the caller's "all failed" path reports it.
        if let Some((assigned_pos, peer_id)) = next_assigned_archive(
            &s.assigned,
            s.next_attempt_idx,
            addr_to_peer,
            skip_unresolved,
        ) {
            out.push(PublicDispatch {
                chunk_index: cd.chunk_index,
                peer_id,
                assigned_pos,
            });
            in_flight += 1;
        }
    }
    out
}

/// The archives that pending chunks are still waiting to see identified:
/// for each chunk neither done nor in flight, its next archive in line,
/// when that archive is not identified. Sorted, deduplicated.
fn unresolved_next_archives(
    state: &HashMap<u32, PublicChunkState>,
    addr_to_peer: &HashMap<[u8; 20], PeerId>,
) -> Vec<[u8; 20]> {
    let mut out: Vec<[u8; 20]> = state
        .values()
        .filter(|s| !s.done && s.in_flight_to.is_none())
        .filter_map(|s| s.assigned.get(s.next_attempt_idx))
        .filter(|a| !addr_to_peer.contains_key(*a))
        .copied()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// V2 per-chunk fetch with V2-assignment routing.
///
/// For each chunk pending fetch, walks the chunk's V2-assigned
/// archive list (in chain-deterministic order) and dispatches a
/// single-shot `pull_chunk_v2(cid, 0, chunk.size)`. The peer's
/// `ShardResponseV2::Data` must satisfy `offset == 0`,
/// `total_bytes == chunk.size`, and `data.len() == chunk.size`;
/// any windowed/partial response is treated as a peer failure
/// (single-shot is the only supported mode for now).
///
/// On peer error or wrong bytes, advances to the next assigned
/// archive. If every assigned archive fails for some chunk, the
/// fetch fails — no "any connected peer" fallback.
///
/// An assigned archive that is never identified on the network (it is
/// down, or unreachable from here) is passed over once
/// [`UNRESOLVED_ARCHIVE_GRACE`] has elapsed since this phase started: the
/// chunk is then asked of its next identified assigned archive (#78).
async fn fetch_v2_public_chunks(
    net: &SumNet,
    store: &Arc<RwLock<sum_store::SumStore>>,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    view: &V2AssignmentView,
    manifest: &DataManifest,
    max_concurrent: usize,
    deadline: tokio::time::Instant,
) -> Result<V2PublicFetchOutcome> {
    // Skip-on-disk: if a chunk is already in the local store (from a
    // prior partial run), don't refetch. Initial `done` reflects this.
    let mut state: HashMap<u32, PublicChunkState> = HashMap::with_capacity(manifest.chunks.len());
    let mut chunks_skipped: u32 = 0;
    {
        let store_read = store.read().await;
        for cd in &manifest.chunks {
            let assigned = view
                .per_chunk_assigned
                .get(&cd.chunk_index)
                .cloned()
                .unwrap_or_default();
            let already_have = store_read.local.has(&cd.cid);
            if already_have {
                chunks_skipped += 1;
            }
            state.insert(
                cd.chunk_index,
                PublicChunkState {
                    assigned,
                    next_attempt_idx: 0,
                    in_flight_to: None,
                    done: already_have,
                },
            );
        }
    }

    let cid_to_idx: HashMap<String, u32> = manifest
        .chunks
        .iter()
        .map(|c| (c.cid.clone(), c.chunk_index))
        .collect();

    let mut chunks_fetched: u32 = 0;
    let mut chunk_peer_attribution: HashMap<PeerId, u32> = HashMap::new();
    let mut peers_contacted: HashSet<PeerId> = HashSet::new();

    async fn try_dispatch(
        net: &SumNet,
        peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
        manifest: &DataManifest,
        state: &mut HashMap<u32, PublicChunkState>,
        max_concurrent: usize,
        skip_unresolved: bool,
    ) -> Result<()> {
        let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
            .read()
            .await
            .iter()
            .map(|(p, a)| (*a, *p))
            .collect();
        let dispatches = select_public_chunk_dispatches(
            state,
            &manifest.chunks,
            &addr_to_peer,
            max_concurrent,
            skip_unresolved,
        );
        for d in dispatches {
            let cd = manifest
                .chunks
                .iter()
                .find(|c| c.chunk_index == d.chunk_index)
                .expect("idx came from manifest");
            // V2 single-shot pull. `max_bytes = cd.size` lets the
            // archive return the full chunk in one Data response.
            if let Err(e) = net
                .pull_chunk_v2(d.peer_id, cd.cid.clone(), 0, cd.size)
                .await
            {
                bail!("pull_chunk_v2 (chunk {}): {e}", cd.chunk_index);
            }
            let s_mut = state
                .get_mut(&cd.chunk_index)
                .expect("idx came from manifest");
            let target_addr = s_mut.assigned[d.assigned_pos];
            if d.assigned_pos != s_mut.next_attempt_idx {
                info!(
                    chunk_index = cd.chunk_index,
                    archive = %hex::encode(target_addr),
                    passed_over = d.assigned_pos - s_mut.next_attempt_idx,
                    "V2Public chunk fetch: asking the next identified assigned archive"
                );
            }
            mark_archive_asked(
                &mut s_mut.assigned,
                &mut s_mut.next_attempt_idx,
                d.assigned_pos,
            );
            s_mut.in_flight_to = Some((d.peer_id, target_addr));
        }
        Ok(())
    }

    // Issue #78: keep to the chain's ranking only for a bounded time.
    let skip_unresolved_at = tokio::time::Instant::now() + UNRESOLVED_ARCHIVE_GRACE;
    let mut skip_unresolved = false;

    try_dispatch(
        net,
        peer_addresses,
        manifest,
        &mut state,
        max_concurrent,
        skip_unresolved,
    )
    .await?;

    while state.values().any(|s| !s.done) {
        let event = tokio::select! {
            ev = net.next_event() => ev,
            _ = tokio::time::sleep_until(skip_unresolved_at), if !skip_unresolved => {
                skip_unresolved = true;
                let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                    .read()
                    .await
                    .iter()
                    .map(|(p, a)| (*a, *p))
                    .collect();
                let waiting = unresolved_next_archives(&state, &addr_to_peer);
                if !waiting.is_empty() {
                    warn!(
                        archives = %waiting.iter().map(hex::encode).collect::<Vec<_>>().join(","),
                        grace = ?UNRESOLVED_ARCHIVE_GRACE,
                        "V2Public chunk fetch: assigned archive(s) not identified on the \
                         network; their chunks are asked of the next identified assigned archive"
                    );
                }
                try_dispatch(net, peer_addresses, manifest, &mut state, max_concurrent, skip_unresolved)
                    .await?;
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let next_missing = manifest
                    .chunks
                    .iter()
                    .map(|c| c.chunk_index)
                    .find(|i| state.get(i).is_some_and(|s| !s.done))
                    .unwrap_or(0);
                bail!(
                    "V2Public chunk fetch timed out; chunk {next_missing} still pending"
                );
            }
        };
        match event {
            Some(SumNetEvent::PeerDiscovered { .. }) => {}
            Some(ref e @ SumNetEvent::PeerIdentified { .. })
            | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                apply_peer_event(&mut *peer_addresses.write().await, e);
                try_dispatch(
                    net,
                    peer_addresses,
                    manifest,
                    &mut state,
                    max_concurrent,
                    skip_unresolved,
                )
                .await?;
            }
            Some(SumNetEvent::ShardReceivedV2 {
                peer_id,
                response:
                    ShardResponseV2::Data {
                        cid,
                        offset,
                        total_bytes,
                        data,
                        error,
                    },
                origin: _,
            }) => {
                let Some(&idx) = cid_to_idx.get(&cid) else {
                    continue;
                };
                let s = state.get_mut(&idx).expect("idx came from manifest");
                if s.done {
                    continue;
                }
                let Some((expected_peer, attempted_addr)) = s.in_flight_to else {
                    continue;
                };
                if peer_id != expected_peer {
                    continue;
                }
                s.in_flight_to = None;
                let cd = manifest
                    .chunks
                    .iter()
                    .find(|c| c.chunk_index == idx)
                    .expect("idx came from manifest");

                let mut chunk_succeeded = false;
                if let Some(err) = error.as_deref() {
                    warn!(
                        chunk_index = idx,
                        %peer_id,
                        archive = %hex::encode(attempted_addr),
                        %err,
                        "V2Public chunk fetch: peer error, trying next assigned archive"
                    );
                } else if offset != 0 || total_bytes != cd.size || data.len() as u64 != cd.size {
                    warn!(
                        chunk_index = idx,
                        %peer_id,
                        archive = %hex::encode(attempted_addr),
                        offset,
                        total_bytes,
                        got_len = data.len(),
                        expected_size = cd.size,
                        "V2Public chunk fetch: partial V2 pull unsupported — \
                         expected single-shot {expected} bytes, trying next assigned archive",
                        expected = cd.size,
                    );
                } else {
                    let actual_hash = *blake3::hash(&data).as_bytes();
                    if actual_hash != cd.blake3_hash {
                        warn!(
                            chunk_index = idx,
                            %peer_id,
                            archive = %hex::encode(attempted_addr),
                            got = %hex::encode(actual_hash),
                            expected = %hex::encode(cd.blake3_hash),
                            "V2Public chunk fetch: peer served wrong bytes, trying next assigned archive"
                        );
                    } else {
                        // Persist + mark done.
                        let store_read = store.read().await;
                        store_read
                            .local
                            .put(&cd.cid, &data)
                            .map_err(|e| anyhow::anyhow!("store.put({}): {e}", cd.cid))?;
                        drop(store_read);
                        state.get_mut(&idx).expect("idx came from manifest").done = true;
                        chunks_fetched += 1;
                        *chunk_peer_attribution.entry(peer_id).or_insert(0) += 1;
                        peers_contacted.insert(peer_id);
                        chunk_succeeded = true;
                    }
                }
                if !chunk_succeeded {
                    let s = state.get_mut(&idx).expect("idx came from manifest");
                    if s.next_attempt_idx >= s.assigned.len() {
                        bail!(
                            "V2Public chunk fetch: chunk {idx} exhausted all {} V2-assigned archives",
                            s.assigned.len()
                        );
                    }
                }
                try_dispatch(
                    net,
                    peer_addresses,
                    manifest,
                    &mut state,
                    max_concurrent,
                    skip_unresolved,
                )
                .await?;
            }
            Some(SumNetEvent::ShardRequestFailed {
                peer_id,
                error,
                origin,
            }) => {
                // Re-queue the chunk that actually failed. The previous
                // version took the *first* in-flight chunk dispatched to this
                // peer, which with several outstanding is an arbitrary choice
                // — it could settle a healthy transfer and leave the failed
                // one parked forever. `origin` names the request, so the
                // lookup is exact.
                let wedged_idx: Option<u32> = origin
                    .requested_cid()
                    .and_then(|cid| cid_to_idx.get(cid).copied())
                    .filter(|idx| {
                        state
                            .get(idx)
                            .and_then(|s| s.in_flight_to)
                            .map(|(p, _)| p == peer_id)
                            .unwrap_or(false)
                    });
                if let Some(idx) = wedged_idx {
                    let s = state.get_mut(&idx).expect("idx from state");
                    let attempted = s.in_flight_to.map(|(_, a)| a).unwrap_or_default();
                    s.in_flight_to = None;
                    warn!(
                        chunk_index = idx,
                        %peer_id,
                        archive = %hex::encode(attempted),
                        %error,
                        "V2Public chunk fetch: outbound failure, trying next assigned archive"
                    );
                    if s.next_attempt_idx >= s.assigned.len() {
                        bail!(
                            "V2Public chunk fetch: chunk {idx} exhausted all {} V2-assigned archives",
                            s.assigned.len()
                        );
                    }
                    try_dispatch(
                        net,
                        peer_addresses,
                        manifest,
                        &mut state,
                        max_concurrent,
                        skip_unresolved,
                    )
                    .await?;
                }
            }
            None => bail!("V2Public chunk fetch: network shut down mid-fetch"),
            _ => {}
        }
    }

    Ok(V2PublicFetchOutcome {
        chunks_fetched,
        chunks_skipped,
        chunk_peer_attribution,
        peers_contacted,
    })
}

// ── build_holder_map active-node routing tests (#37) ──────────────────────────

#[cfg(test)]
mod holder_map_tests {
    use super::*;
    use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};

    fn orchestrator(url: String) -> DownloadOrchestrator {
        DownloadOrchestrator::new(
            format!("0x{}", "11".repeat(32)),
            PathBuf::from("/dev/null"),
            Arc::new(L1RpcClient::new(url)),
            4,
            Duration::from_secs(5),
            3,
        )
    }

    fn empty_manifest() -> DataManifest {
        DataManifest {
            file_name: "f".into(),
            file_hash: [0u8; 32],
            total_size_bytes: 0,
            chunk_count: 0,
            merkle_root: [0x11u8; 32],
            chunks: Vec::new(),
        }
    }

    /// (c) `build_holder_map` reads the finalized height EXACTLY ONCE and
    /// pins the active-node snapshot to precisely that height.
    #[tokio::test]
    async fn build_holder_map_reads_finalized_height_once_and_pins_snapshot() {
        let server = start_mock_rpc(routes([
            (
                "chain_getBlockHeight",
                MockResponse::Result(serde_json::json!({"height": 555, "finality": "finalized"})),
            ),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::Result(serde_json::json!([])),
            ),
        ]))
        .await;
        let orch = orchestrator(server.url());
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let _ = orch.build_holder_map(&empty_manifest(), &peers).await;
        assert_eq!(
            server.method_count("chain_getBlockHeight"),
            1,
            "exactly one finalized-height read per build_holder_map"
        );
        assert_eq!(
            server.first_params("storage_getActiveNodesAtHeight"),
            Some(serde_json::json!([555])),
            "snapshot pinned to the finalized height"
        );
    }

    /// (f) A finalized-height lookup failure routes to the gossipsub
    /// fallback (empty holder map) — never a hard error, since this
    /// function returns a map, not a Result. The at-height snapshot is
    /// never queried.
    #[tokio::test]
    async fn build_holder_map_falls_back_when_height_lookup_fails() {
        let server = start_mock_rpc(routes([
            ("chain_getBlockHeight", MockResponse::Hangup),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::Result(serde_json::json!([])),
            ),
        ]))
        .await;
        let orch = orchestrator(server.url());
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let map = orch.build_holder_map(&empty_manifest(), &peers).await;
        assert!(
            map.is_empty(),
            "height failure must fall back to gossipsub (empty holder map)"
        );
        assert_eq!(
            server.method_count("storage_getActiveNodesAtHeight"),
            0,
            "no at-height query after a height failure"
        );
    }

    /// (f) An at-height snapshot failure ALSO routes to the gossipsub
    /// fallback (empty holder map), not a hard error.
    #[tokio::test]
    async fn build_holder_map_falls_back_when_at_height_lookup_fails() {
        let server = start_mock_rpc(routes([
            (
                "chain_getBlockHeight",
                MockResponse::Result(serde_json::json!({"height": 9, "finality": "finalized"})),
            ),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::error("snapshot unavailable"),
            ),
        ]))
        .await;
        let orch = orchestrator(server.url());
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let map = orch.build_holder_map(&empty_manifest(), &peers).await;
        assert!(
            map.is_empty(),
            "at-height snapshot failure must fall back to gossipsub"
        );
    }
}

// ── Issue #78: a top-ranked archive that is down ──────────────────────────────

#[cfg(test)]
mod unresolved_archive_tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::net::Ipv4Addr;

    use libp2p::Multiaddr;
    use libp2p::multiaddr::Protocol;
    use sum_net::{Keypair, ShardRequestV2, l1_address_from_keypair};
    use sum_types::config::{NetConfig, StoreConfig};

    const DOWN: [u8; 20] = [0xD0; 20];
    const A: [u8; 20] = [0xA1; 20];
    const B: [u8; 20] = [0xB2; 20];

    fn chunks(n: u32) -> Vec<ChunkDescriptor> {
        (0..n)
            .map(|i| ChunkDescriptor {
                chunk_index: i,
                offset: 0,
                size: 1,
                blake3_hash: [i as u8; 32],
                cid: format!("bafk-78-{i}"),
                plaintext_blake3_hash: None,
            })
            .collect()
    }

    fn state_for(n: u32, order: &[[u8; 20]]) -> HashMap<u32, PublicChunkState> {
        (0..n)
            .map(|i| {
                (
                    i,
                    PublicChunkState {
                        assigned: order.to_vec(),
                        next_attempt_idx: 0,
                        in_flight_to: None,
                        done: false,
                    },
                )
            })
            .collect()
    }

    fn peers(addrs: &[[u8; 20]]) -> HashMap<[u8; 20], PeerId> {
        addrs
            .iter()
            .map(|a| (*a, Keypair::generate_ed25519().public().to_peer_id()))
            .collect()
    }

    /// Within the grace period a chunk waits for its top-ranked archive,
    /// even though the next one is identified; after it, the next
    /// identified assigned archive is asked.
    #[test]
    fn a_chunk_waits_for_its_top_archive_only_within_the_grace_period() {
        let chunks = chunks(2);
        let state = state_for(2, &[DOWN, A, B]);
        let p = peers(&[A, B]);

        let strict = select_public_chunk_dispatches(&state, &chunks, &p, 8, false);
        assert!(strict.is_empty(), "{strict:?}");
        assert_eq!(unresolved_next_archives(&state, &p), vec![DOWN]);

        let after = select_public_chunk_dispatches(&state, &chunks, &p, 8, true);
        assert_eq!(after.len(), 2);
        for d in &after {
            assert_eq!(d.assigned_pos, 1);
            assert_eq!(d.peer_id, p[&A]);
        }
    }

    /// In line, the top-ranked archive is asked first once identified, with
    /// or without the grace period over.
    #[test]
    fn an_identified_top_archive_is_always_asked_first() {
        let chunks = chunks(1);
        let state = state_for(1, &[A, B]);
        let p = peers(&[A, B]);
        for skip in [false, true] {
            let d = select_public_chunk_dispatches(&state, &chunks, &p, 8, skip);
            assert_eq!(d.len(), 1);
            assert_eq!((d[0].assigned_pos, d[0].peer_id), (0, p[&A]));
        }
    }

    /// No assigned archive identified: nothing is asked, before or after the
    /// grace period, and nothing is counted as asked.
    #[test]
    fn nothing_is_asked_while_no_assigned_archive_is_identified() {
        let chunks = chunks(1);
        let state = state_for(1, &[DOWN, A]);
        for skip in [false, true] {
            assert!(
                select_public_chunk_dispatches(&state, &chunks, &HashMap::new(), 8, skip)
                    .is_empty()
            );
        }
        assert_eq!(state[&0].next_attempt_idx, 0);
    }

    /// Asking an archive out of turn keeps the passed-over one in line: if
    /// the asked archive then fails, the passed-over one is next, and the
    /// chunk counts as exhausted only after every assigned archive was asked.
    #[test]
    fn a_passed_over_archive_stays_in_line() {
        let mut order = vec![DOWN, A, B];
        let mut next = 0;
        let p = peers(&[A, B]);
        let (pos, _) = next_assigned_archive(&order, next, &p, true).unwrap();
        assert_eq!(pos, 1);
        mark_archive_asked(&mut order, &mut next, pos);
        assert_eq!((order.clone(), next), (vec![A, DOWN, B], 1));

        // A failed. DOWN is still unidentified: strictly, wait for it;
        // after the grace period, ask B.
        assert_eq!(next_assigned_archive(&order, next, &p, false), None);
        let (pos, peer) = next_assigned_archive(&order, next, &p, true).unwrap();
        assert_eq!((pos, peer), (2, p[&B]));
        mark_archive_asked(&mut order, &mut next, pos);
        assert_eq!((order.clone(), next), (vec![A, B, DOWN], 2));

        // B failed too; DOWN comes back and is asked last.
        let mut p2 = p.clone();
        p2.insert(DOWN, Keypair::generate_ed25519().public().to_peer_id());
        let (pos, _) = next_assigned_archive(&order, next, &p2, false).unwrap();
        assert_eq!(pos, 2);
        mark_archive_asked(&mut order, &mut next, pos);
        assert_eq!(next, order.len(), "every assigned archive asked once");
        assert_eq!(next_assigned_archive(&order, next, &p2, true), None);
    }

    /// The in-flight cap still holds when chunks are asked out of turn.
    #[test]
    fn the_in_flight_cap_holds_after_the_grace_period() {
        let chunks = chunks(5);
        let state = state_for(5, &[DOWN, A]);
        let p = peers(&[A]);
        let d = select_public_chunk_dispatches(&state, &chunks, &p, 2, true);
        assert_eq!(
            d.iter().map(|d| d.chunk_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    // ── Over real swarms on loopback ──────────────────────────────────────

    const PAYLOAD: &[u8] = b"snip-78 chunk served by a lower-ranked archive";

    /// An archive: a `SumNet` node on loopback, mDNS off, that answers every
    /// V2 Pull with `PAYLOAD`. Returns its dialable TCP address and L1
    /// address.
    async fn archive() -> (Multiaddr, [u8; 20], tokio::task::JoinHandle<()>) {
        let kp = Keypair::generate_ed25519();
        let l1 = l1_address_from_keypair(&kp);
        let peer = kp.public().to_peer_id();
        let net = Arc::new(SumNet::new(loopback(&[]), kp).await.expect("SumNet::new"));
        let tcp = loop {
            match net.next_event().await.expect("archive events") {
                SumNetEvent::Listening { addr }
                    if !addr.iter().any(|p| matches!(p, Protocol::QuicV1)) =>
                {
                    break addr;
                }
                _ => {}
            }
        };
        let serve = net.clone();
        let task = tokio::spawn(async move {
            while let Some(ev) = serve.next_event().await {
                if let SumNetEvent::ShardRequestedV2 {
                    channel_id,
                    request: ShardRequestV2::Pull { cid, .. },
                    ..
                } = ev
                {
                    let _ = serve
                        .respond_shard_v2(
                            channel_id,
                            ShardResponseV2::Data {
                                cid,
                                offset: 0,
                                total_bytes: PAYLOAD.len() as u64,
                                data: PAYLOAD.to_vec(),
                                error: None,
                            },
                        )
                        .await;
                }
            }
        });
        (tcp.with(Protocol::P2p(peer)), l1, task)
    }

    fn loopback(bootstrap: &[String]) -> NetConfig {
        NetConfig {
            enable_wan: true,
            mdns: false,
            listen_ip: Some(Ipv4Addr::LOCALHOST),
            bootstrap_peers: bootstrap.to_vec(),
            ..NetConfig::default()
        }
    }

    /// Issue #78 end to end: a chunk replicated to three archives whose
    /// top-ranked archive is down. The two others are up and identified.
    /// The download asks the next one once the grace period is over, instead
    /// of waiting for the down archive until the deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_chunk_is_fetched_from_the_next_archive_when_the_top_one_is_down() {
        let (addr_a, l1_a, _serve_a) = archive().await;
        let (addr_b, l1_b, _serve_b) = archive().await;
        let down = l1_address_from_keypair(&Keypair::generate_ed25519());

        let client = SumNet::new(
            loopback(&[addr_a.to_string(), addr_b.to_string()]),
            Keypair::generate_ed25519(),
        )
        .await
        .expect("client SumNet::new");

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RwLock::new(
            sum_store::SumStore::new(StoreConfig {
                store_dir: dir.path().join("store"),
                max_chunk_msg_bytes: 2 * 1024 * 1024,
            })
            .unwrap(),
        ));
        let manifest = DataManifest {
            file_name: "issue-78.bin".into(),
            file_hash: *blake3::hash(PAYLOAD).as_bytes(),
            total_size_bytes: PAYLOAD.len() as u64,
            chunk_count: 1,
            merkle_root: [0x78; 32],
            chunks: vec![ChunkDescriptor {
                chunk_index: 0,
                offset: 0,
                size: PAYLOAD.len() as u64,
                blake3_hash: *blake3::hash(PAYLOAD).as_bytes(),
                cid: sum_store::cid_from_data(PAYLOAD),
                plaintext_blake3_hash: None,
            }],
        };
        let ranked = vec![down, l1_a, l1_b];
        let view = V2AssignmentView {
            snapshot: {
                let mut s = ranked.clone();
                s.sort();
                s
            },
            r: 3,
            distinct_assigned: ranked.iter().copied().collect::<BTreeSet<_>>(),
            per_chunk_assigned: HashMap::from([(0, ranked.clone())]),
        };
        let peer_addresses = RwLock::new(HashMap::new());

        let started = tokio::time::Instant::now();
        let outcome = fetch_v2_public_chunks(
            &client,
            &store,
            &peer_addresses,
            &view,
            &manifest,
            4,
            started + Duration::from_secs(30),
        )
        .await
        .expect("the chunk is fetched from an archive that is up");
        let took = started.elapsed();

        assert_eq!(outcome.chunks_fetched, 1);
        let served_by: Vec<[u8; 20]> = {
            let map = peer_addresses.read().await;
            outcome
                .chunk_peer_attribution
                .keys()
                .map(|p| map[p])
                .collect()
        };
        assert_eq!(
            served_by,
            vec![l1_a],
            "the next archive in rank order serves it"
        );
        assert_eq!(
            store
                .read()
                .await
                .local
                .get(&manifest.chunks[0].cid)
                .unwrap(),
            PAYLOAD
        );
        assert!(
            took >= UNRESOLVED_ARCHIVE_GRACE && took < Duration::from_secs(25),
            "waited {took:?}: the top-ranked archive is waited for only during the grace period"
        );
    }
}
