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
use sum_store::manifest::deserialize_manifest_cbor;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;
use sum_store::{
    FetchManager, FetchOutcome, MerkleTree, compute_chunk_assignment, nodes_for_chunk,
};
use sum_types::rpc_types::StorageFileInfoV2;
use sum_types::storage::DataManifest;

use crate::download_v2_routing::{
    ManifestDecodeError, V2AssignmentError, V2AssignmentView, build_v2_assignment_view,
    decode_v2_manifest_bytes,
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
        let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", self.merkle_root_hex);
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
                        Some(SumNetEvent::ShardReceived { peer_id, response })
                            if response.cid == manifest_cid =>
                        {
                            if let Some(ref err) = response.error {
                                // The peer we asked answered "not found".
                                // Don't bail — keep waiting for another
                                // peer's response. New peers discovered
                                // below will get their own request.
                                warn!(%peer_id, %err, "manifest request rejected by a peer — waiting for others");
                                continue;
                            }
                            let m = deserialize_manifest_cbor(&response.data)
                                .map_err(|e| anyhow::anyhow!("failed to deserialize manifest: {e}"))?;
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
                        Some(SumNetEvent::ShardRequestFailed { peer_id, error })
                            // An outbound request to `peer_id` failed before
                            // it could be sent (no connection, peer down,
                            // protocol mismatch). During Phase 2 the only
                            // outbound traffic is manifest requests, so
                            // treat this as a manifest failure for that
                            // peer. We don't retry against the same peer —
                            // we wait for another peer to be discovered.
                            if tried_for_manifest.contains(&peer_id) =>
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
                        Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                            // Skip manifest responses
                            if response.cid.starts_with(MANIFEST_REQUEST_PREFIX) {
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
                        Some(SumNetEvent::ShardRequestFailed { peer_id, error }) => {
                            // Re-queue every chunk whose outbound request was
                            // attributed to this peer. Without this, a relay
                            // circuit reset (or any transport-layer failure)
                            // permanently wedges the pipeline: in_flight stays
                            // full, fill_fetches refuses to issue more, and the
                            // download times out with 0 progress.
                            let wedged: Vec<String> = in_flight
                                .iter()
                                .filter(|(_, p)| **p == peer_id)
                                .map(|(cid, _)| cid.clone())
                                .collect();
                            warn!(
                                %peer_id,
                                %error,
                                requeued = wedged.len(),
                                "shard request failed — re-queuing in-flight chunks from peer"
                            );
                            for cid in &wedged {
                                in_flight.remove(cid);
                                if let Some(chunk) = manifest.chunks.iter().find(|c| &c.cid == cid) {
                                    remaining.push_back(chunk.chunk_index);
                                }
                            }
                            if !wedged.is_empty() {
                                self.fill_fetches(
                                    &net, &mut fetcher, &mut remaining, &mut in_flight,
                                    &manifest, &holder_map, &discovered_peers,
                                ).await;
                            }
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
        let nodes_result = self.rpc.get_active_nodes().await;
        let Ok(node_records) = nodes_result else {
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
            Some(SumNetEvent::ShardRequestFailed { peer_id, error }) => {
                if let Some(&archive) = dispatched_peers.get(&peer_id) {
                    if status.get(&archive) == Some(&Status::Dispatched) {
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
async fn fetch_v2_public_chunks(
    net: &SumNet,
    store: &Arc<RwLock<sum_store::SumStore>>,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    view: &V2AssignmentView,
    manifest: &DataManifest,
    max_concurrent: usize,
    deadline: tokio::time::Instant,
) -> Result<V2PublicFetchOutcome> {
    // Per-chunk state. `assigned` is the V2-deterministic order;
    // `next_attempt_idx` walks it on each failure; `in_flight_to`
    // pins the (peer_id, archive) we're awaiting; `received` is set
    // when the chunk is persisted to local disk.
    struct ChunkState {
        assigned: Vec<[u8; 20]>,
        next_attempt_idx: usize,
        in_flight_to: Option<(PeerId, [u8; 20])>,
        done: bool,
    }

    // Skip-on-disk: if a chunk is already in the local store (from a
    // prior partial run), don't refetch. Initial `done` reflects this.
    let mut state: HashMap<u32, ChunkState> = HashMap::with_capacity(manifest.chunks.len());
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
                ChunkState {
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
        state: &mut HashMap<u32, ChunkState>,
        max_concurrent: usize,
    ) -> Result<()> {
        let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
            .read()
            .await
            .iter()
            .map(|(p, a)| (*a, *p))
            .collect();
        let cap = max_concurrent.max(1);
        let mut in_flight = state.values().filter(|s| s.in_flight_to.is_some()).count();
        for cd in &manifest.chunks {
            if in_flight >= cap {
                break;
            }
            let Some(s) = state.get(&cd.chunk_index) else {
                continue;
            };
            if s.done || s.in_flight_to.is_some() {
                continue;
            }
            let probe_idx = s.next_attempt_idx;
            if probe_idx >= s.assigned.len() {
                // No more assigned archives to try for this chunk —
                // surfaced by the caller's "all failed" path.
                continue;
            }
            let target_addr = s.assigned[probe_idx];
            let Some(&peer_id) = addr_to_peer.get(&target_addr) else {
                // Not yet resolvable; wait for PeerIdentified.
                continue;
            };
            // V2 single-shot pull. `max_bytes = cd.size` lets the
            // archive return the full chunk in one Data response.
            if let Err(e) = net.pull_chunk_v2(peer_id, cd.cid.clone(), 0, cd.size).await {
                bail!("pull_chunk_v2 (chunk {}): {e}", cd.chunk_index);
            }
            let s_mut = state
                .get_mut(&cd.chunk_index)
                .expect("idx came from manifest");
            s_mut.in_flight_to = Some((peer_id, target_addr));
            s_mut.next_attempt_idx += 1;
            in_flight += 1;
        }
        Ok(())
    }

    try_dispatch(net, peer_addresses, manifest, &mut state, max_concurrent).await?;

    while state.values().any(|s| !s.done) {
        let event = tokio::select! {
            ev = net.next_event() => ev,
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
                try_dispatch(net, peer_addresses, manifest, &mut state, max_concurrent).await?;
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
                try_dispatch(net, peer_addresses, manifest, &mut state, max_concurrent).await?;
            }
            Some(SumNetEvent::ShardRequestFailed { peer_id, error }) => {
                // Re-queue the chunk whose dispatch was attributed to
                // this peer. Same semantics as the V1 orchestrator's
                // wedging guard — without this, a transport reset
                // permanently parks the chunk.
                let wedged_idx: Option<u32> =
                    state.iter().find_map(|(idx, s)| match s.in_flight_to {
                        Some((p, _)) if p == peer_id => Some(*idx),
                        _ => None,
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
                    try_dispatch(net, peer_addresses, manifest, &mut state, max_concurrent).await?;
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
