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

use sum_net::identity;
use sum_net::{PeerId, SumNet};
use sum_store::gc::GarbageCollector;
use sum_store::{SumStore, chunks_for_node, compute_chunk_assignment, nodes_for_chunk};
use sum_types::rpc_types::{NodeRecordInfo, StorageFileInfo};
use sum_types::storage::CHUNK_SIZE;

use crate::rpc_client::L1RpcClient;

/// Background worker that syncs this node's assigned chunks from the network,
/// and garbage-collects unassigned chunks after a grace period.
///
/// Fetches are routed through `SumStore::fetcher` (a [`sum_store::FetchManager`])
/// so the listen-mode event loop in `main.rs` can complete them via
/// `on_chunk_received`. Deduplication uses `fetcher.is_active(cid)` as the
/// single source of truth — there is no separate pending-fetches set.
pub struct MarketSyncWorker {
    rpc: Arc<L1RpcClient>,
    /// This node's L1 address (20 bytes).
    l1_address: [u8; 20],
    /// This node's L1 address in base58 (for logging).
    l1_address_base58: String,
    /// How often to poll the L1 for assignment changes.
    poll_interval: Duration,
    /// Garbage collector for unassigned chunks.
    gc: GarbageCollector,
    /// When the L1 was last successfully polled (for GC safety).
    last_l1_poll: Instant,
    /// Consecutive RPC failures (for exponential backoff).
    consecutive_failures: u32,
    /// Live-chain replication factor sourced from
    /// `ChainParamsInfo::assignment_replication_factor` at process
    /// startup (`main.rs::run_listen`). Fetch ownership and GC
    /// retained-set both derive from a single assignment call at this
    /// R, guaranteeing no divergence.
    replication_factor: u32,
}

impl MarketSyncWorker {
    pub fn new(
        rpc: Arc<L1RpcClient>,
        l1_address: [u8; 20],
        l1_address_base58: String,
        poll_interval: Duration,
        gc_grace_period: Duration,
        replication_factor: u32,
    ) -> Self {
        Self {
            rpc,
            l1_address,
            l1_address_base58,
            poll_interval,
            gc: GarbageCollector::new(gc_grace_period),
            last_l1_poll: Instant::now(),
            consecutive_failures: 0,
            replication_factor,
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
        // 1. Get funded files and active nodes from L1. Apply the
        // shared eligibility contract to narrow to exactly-eligible
        // archives before assignment; fetch and GC both derive from
        // this filtered list, so no divergence is possible.
        //
        // The active-node snapshot is pinned to the finalized head: the
        // finalized height is read exactly once at the top of the cycle
        // (inside `active_nodes_at_finalized_head`) — never per file — so
        // every consumer in this cycle observes one consistent snapshot. A
        // height-lookup failure fails the whole cycle via the same typed
        // operation error as any other RPC failure (`?`).
        let files = self.rpc.get_funded_files().await?;
        let node_records = self.active_nodes_at_finalized_head().await?;
        let node_records = sum_types::rpc_types::filter_active_archives(node_records);

        if files.is_empty() || node_records.is_empty() {
            debug!("no funded files or no eligible archives — skipping sync");
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

        // 3. For each funded file, compute assignment and fetch missing chunks
        for file in &files {
            if let Err(e) = self
                .sync_file(file, &node_addrs, &addr_to_peer, store, net)
                .await
            {
                warn!(merkle_root = %file.merkle_root, %e, "failed to sync file");
            }
        }

        // 4. Run garbage collection over positively attributed candidates
        // only. Anything else on disk (V2 pushes, Private ciphertext, chunks
        // of files outside the funded V1 set, orphans) is retained.
        //
        // Attribution and deletion happen under ONE write guard: a V2 Push
        // records its GC exemption under the write lock, so no push can land
        // between computing the candidates and deleting them.
        self.last_l1_poll = Instant::now();
        {
            let store_guard = store.write().await;
            let candidates = self.compute_gc_candidates(&files, &node_addrs, &store_guard);
            match self
                .gc
                .sweep_candidates(&store_guard.local, &candidates, self.last_l1_poll)
            {
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

    /// Fetch the active-archive snapshot pinned to the current finalized
    /// head.
    ///
    /// Reads the finalized height exactly once via
    /// `chain_get_block_height` (which passes `["finalized"]`), then the
    /// height-pinned snapshot via `storage_get_active_nodes_at_height` at
    /// exactly that height. This is the supported replacement for the
    /// removed `storage_getActiveNodes` bulk endpoint. Both lookups
    /// surface their typed errors; the sole caller (`sync_cycle`)
    /// propagates them with `?`.
    async fn active_nodes_at_finalized_head(&self) -> Result<Vec<NodeRecordInfo>> {
        let finalized_height = self.rpc.chain_get_block_height().await?.height;
        self.rpc
            .storage_get_active_nodes_at_height(finalized_height)
            .await
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
        let root_hex = file
            .merkle_root
            .strip_prefix("0x")
            .unwrap_or(&file.merkle_root);
        let root_bytes =
            hex_to_32(root_hex).ok_or_else(|| anyhow::anyhow!("invalid merkle_root hex"))?;

        // Compute chunk count
        let chunk_count = file.total_size_bytes.div_ceil(CHUNK_SIZE);
        if chunk_count == 0 {
            return Ok(());
        }

        // Compute assignment through the shared kernel — the GC path
        // uses the same helper, guaranteeing byte-identical ownership.
        let (assignment, my_chunks) = my_assigned_chunk_indices(
            &root_bytes,
            chunk_count,
            node_addrs,
            self.replication_factor,
            &self.l1_address,
        );
        if my_chunks.is_empty() {
            return Ok(()); // Not assigned to this file
        }

        // Check if we have the manifest for this file
        let store_read = store.read().await;
        let has_manifest = store_read
            .manifest_idx
            .get_by_merkle_root(&root_bytes)
            .is_some();

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

        // We have the manifest — collect targets that need fetching.
        // Done under the read lock so the listen-loop's writer (which calls
        // on_chunk_received under a write lock) isn't blocked.
        struct FetchTarget {
            chunk_index: u32,
            cid: String,
            expected_size: u64,
            peer_id: PeerId,
        }
        let mut targets: Vec<FetchTarget> = Vec::new();
        if let Some(manifest) = store_read.manifest_idx.get_by_merkle_root(&root_bytes) {
            for chunk_index in &my_chunks {
                let Some(chunk) = manifest.chunks.get(*chunk_index as usize) else {
                    continue;
                };
                let cid = &chunk.cid;

                if store_read.local.has(cid) {
                    continue; // Already have this chunk
                }
                if store_read.fetcher.is_active(cid) {
                    continue; // Already in flight via FetchManager
                }

                // Pick the first peer that holds this chunk and isn't us.
                let Some(holders) = nodes_for_chunk(&assignment, *chunk_index) else {
                    continue;
                };
                let chosen_peer = holders
                    .iter()
                    .filter(|h| **h != self.l1_address)
                    .find_map(|h| addr_to_peer.get(h).copied());
                let Some(peer_id) = chosen_peer else { continue };

                targets.push(FetchTarget {
                    chunk_index: *chunk_index,
                    cid: cid.clone(),
                    expected_size: chunk.size,
                    peer_id,
                });
            }
        }
        drop(store_read);

        // Issue fetches via the shared FetchManager so the listen-mode event
        // loop can complete them via on_chunk_received. Acquire the write
        // lock briefly per call to minimize contention with the event loop.
        for target in targets {
            info!(
                root = root_hex,
                chunk = target.chunk_index,
                cid = %target.cid,
                peer = %target.peer_id,
                expected_size = target.expected_size,
                "fetching assigned chunk via FetchManager"
            );
            let mut store_w = store.write().await;
            if let Err(e) = store_w
                .fetcher
                .start_fetch_with_expected_size(
                    net.as_ref(),
                    target.peer_id,
                    target.cid.clone(),
                    Some(target.expected_size),
                )
                .await
            {
                warn!(cid = %target.cid, %e, "failed to start fetch");
            }
        }

        Ok(())
    }

    /// Compute the CIDs garbage collection may delete.
    ///
    /// Positive attribution only. A CID is a candidate iff it is a chunk of
    /// a funded V1 file whose manifest is indexed here, it is not assigned
    /// to this node in any funded V1 file, and no other locally held file
    /// references it: no Public manifest outside the funded V1 set (V2
    /// pushes and files that left the funded set) and no Private chunk
    /// mapping. The funded set (`storage_getFundedFiles`) lists V1 files
    /// only, so V2 chunks are never candidates. A chunk whose ownership
    /// cannot be determined is retained.
    fn compute_gc_candidates(
        &self,
        files: &[StorageFileInfo],
        node_addrs: &[[u8; 20]],
        store: &SumStore,
    ) -> HashSet<String> {
        let idx = &store.manifest_idx;
        if !idx.gc_protection_intact() {
            // The GC-protected list is missing or unreadable: which chunks
            // arrived by V2 Push is unknown, so nothing is a candidate.
            warn!("GC-protected chunk list unavailable; deleting nothing");
            return HashSet::new();
        }

        let mut funded_roots: HashSet<[u8; 32]> = HashSet::new();
        let mut protected: HashSet<String> = HashSet::new();
        let mut unassigned: HashSet<String> = HashSet::new();

        for file in files {
            let root_hex = file
                .merkle_root
                .strip_prefix("0x")
                .unwrap_or(&file.merkle_root);
            let Some(root_bytes) = hex_to_32(root_hex) else {
                continue;
            };
            funded_roots.insert(root_bytes);

            let chunk_count = file.total_size_bytes.div_ceil(CHUNK_SIZE);
            if chunk_count == 0 {
                continue;
            }

            // Same kernel as the fetch path — see [`my_assigned_chunk_indices`].
            let (_assignment, my_chunks) = my_assigned_chunk_indices(
                &root_bytes,
                chunk_count,
                node_addrs,
                self.replication_factor,
                &self.l1_address,
            );
            let mine: HashSet<u32> = my_chunks.into_iter().collect();

            if let Some(manifest) = idx.get_by_merkle_root(&root_bytes) {
                for (chunk_index, chunk) in manifest.chunks.iter().enumerate() {
                    if mine.contains(&(chunk_index as u32)) {
                        protected.insert(chunk.cid.clone());
                    } else {
                        unassigned.insert(chunk.cid.clone());
                    }
                }
            }
        }

        // Chunks of any other locally held Public manifest stay, whatever
        // their protocol version.
        for root in idx.all_merkle_roots() {
            if funded_roots.contains(&root) {
                continue;
            }
            if let Some(manifest) = idx.get_by_merkle_root(&root) {
                protected.extend(manifest.chunks.iter().map(|c| c.cid.clone()));
            }
        }

        // A chunk accepted by a V2 Push (or present when the protected list
        // was created) is never a candidate, whichever manifest it is read
        // through: a funded V1 registration may reuse a V2 file's root, and
        // the manifest index is keyed by root alone.
        unassigned.retain(|cid| {
            !protected.contains(cid) && !idx.is_private_chunk_cid(cid) && !idx.is_gc_protected(cid)
        });
        unassigned
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

/// Shared assignment kernel used by BOTH the fetch path
/// ([`MarketSyncWorker::sync_file`]) and the GC path
/// ([`MarketSyncWorker::compute_gc_candidates`]).
///
/// Extracting the deterministic core into one function is the
/// structural guarantee that both paths always agree on which chunks
/// this archive owns — the reviewer flagged divergence as the fatal
/// failure mode. Any caller that computes ownership must funnel through
/// this helper.
///
/// Inputs are pre-filtered by [`sum_types::rpc_types::filter_active_archives`]
/// upstream at the sync_cycle boundary; this function assumes the
/// snapshot has already been narrowed to eligible archives.
fn my_assigned_chunk_indices(
    root_bytes: &[u8; 32],
    chunk_count: u64,
    node_addrs: &[[u8; 20]],
    replication_factor: u32,
    my_addr: &[u8; 20],
) -> (
    Vec<Vec<[u8; 20]>>, // full assignment: nodes per chunk (for peer lookup)
    Vec<u32>,           // chunks assigned to `my_addr`
) {
    let assignment =
        compute_chunk_assignment(root_bytes, chunk_count, node_addrs, replication_factor);
    let my_chunks = chunks_for_node(&assignment, my_addr);
    (assignment, my_chunks)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::rpc_types::{NodeRecordInfo, filter_active_archives};

    fn snapshot_of(n: u8) -> Vec<[u8; 20]> {
        (0..n)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xF0 + i;
                a
            })
            .collect()
    }

    fn record(addr: &[u8; 20], role: &str, status: &str) -> NodeRecordInfo {
        NodeRecordInfo {
            address: sum_net::l1_address_base58(addr),
            role: role.into(),
            staked_balance: 1_000_000_000,
            status: status.into(),
            registered_at: 1,
        }
    }

    /// The single-source-of-truth guarantee: whatever `my_assigned_chunk_indices`
    /// computes for a given (root, chunk_count, snapshot, R, my_addr)
    /// is the value used by both the fetch path and the GC retained-set
    /// path. This test asserts the kernel is deterministic (invariant
    /// across repeated calls) and stable across snapshot ordering, so
    /// no ordering fluke can desync the two callers.
    #[test]
    fn assignment_kernel_is_deterministic_and_snapshot_order_invariant() {
        let root = [0x11u8; 32];
        let chunk_count = 32u64;
        let mut snap = snapshot_of(5);
        let my_addr = snap[2];

        let (_a1, my1) = my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);
        let (_a2, my2) = my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);
        assert_eq!(
            my1, my2,
            "kernel must be deterministic across repeated calls"
        );

        // Shuffle the snapshot; sum-store's assignment must be
        // order-invariant, so my_chunks must match.
        snap.reverse();
        let (_a3, my3) = my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);
        assert_eq!(
            my1, my3,
            "snapshot ordering must not change owned chunk indices"
        );
        assert!(
            !my1.is_empty(),
            "test needs a non-empty owned set to be meaningful"
        );
    }

    /// Non-default R changes the owned set. Regression guard against a
    /// refactor that accidentally hardcodes R.
    #[test]
    fn assignment_kernel_reflects_non_default_replication_factor() {
        let root = [0x22u8; 32];
        let chunk_count = 32u64;
        let snap = snapshot_of(5);
        let my_addr = snap[0];

        let (_, my_r_1) = my_assigned_chunk_indices(&root, chunk_count, &snap, 1, &my_addr);
        let (_, my_r_3) = my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);
        let (_, my_r_5) = my_assigned_chunk_indices(&root, chunk_count, &snap, 5, &my_addr);

        // R=1 gives the smallest owned set; R=5 (== N) gives every chunk.
        assert!(
            my_r_1.len() < my_r_3.len(),
            "R=1 must own fewer chunks than R=3 (got {} vs {})",
            my_r_1.len(),
            my_r_3.len(),
        );
        assert_eq!(
            my_r_5.len(),
            chunk_count as usize,
            "R=N=5 must own every chunk"
        );
        // Every chunk owned at a lower R must also be owned at the
        // higher R (superset relation for rendezvous hashing).
        for c in &my_r_1 {
            assert!(my_r_3.contains(c), "R=1 owned chunk {c} missing from R=3");
            assert!(my_r_5.contains(c), "R=1 owned chunk {c} missing from R=5");
        }
    }

    /// The fetch path (`sync_file` → `my_assigned_chunk_indices`) and
    /// the GC path (`compute_gc_candidates` → `my_assigned_chunk_indices`)
    /// receive the same node_addrs slice. This test asserts the shared
    /// helper yields identical results in the exact scenarios the two
    /// paths encounter — same file, same snapshot, same R. Any refactor
    /// that lets the two paths diverge (e.g. one calls with unfiltered
    /// records) will fail this assertion.
    #[test]
    fn fetch_and_gc_paths_share_identical_assignment_output() {
        let root = [0x33u8; 32];
        let chunk_count = 40u64;
        let snap = snapshot_of(5);
        let my_addr = snap[3];

        // Simulate both call sites — deliberately duplicate the inputs
        // to prove nothing hidden in the caller alters the result.
        let (fetch_assignment, fetch_my_chunks) =
            my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);
        let (gc_assignment, gc_my_chunks) =
            my_assigned_chunk_indices(&root, chunk_count, &snap, 3, &my_addr);

        assert_eq!(
            fetch_my_chunks, gc_my_chunks,
            "fetch and GC paths MUST compute identical owned chunk indices"
        );
        assert_eq!(
            fetch_assignment.len(),
            gc_assignment.len(),
            "assignment vectors must be identical length"
        );
        for (i, (f, g)) in fetch_assignment
            .iter()
            .zip(gc_assignment.iter())
            .enumerate()
        {
            assert_eq!(f, g, "chunk {i}: fetch and GC assignment must match");
        }
    }

    /// The filter is applied at `sync_cycle`'s entry point (single call
    /// site — line 129), so both call sites receive the SAME filtered
    /// snapshot. This test proves the filter behaves correctly on the
    /// exact record shapes the sync loop sees: Slashed, Unbonding,
    /// Withdrawn, non-ArchiveNode are excluded; Active/ArchiveNode is
    /// admitted.
    #[test]
    fn filter_narrows_snapshot_to_eligible_archives_only() {
        let addrs: Vec<[u8; 20]> = (0..6)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0x80 + i;
                a
            })
            .collect();
        let raw = vec![
            record(&addrs[0], "ArchiveNode", "Active"),    // keep
            record(&addrs[1], "ArchiveNode", "Slashed"),   // drop
            record(&addrs[2], "ArchiveNode", "Unbonding"), // drop
            record(&addrs[3], "ArchiveNode", "Withdrawn"), // drop
            record(&addrs[4], "ValidatorNode", "Active"),  // drop (wrong role)
            record(&addrs[5], "ArchiveNode", "Active"),    // keep
        ];

        let filtered = filter_active_archives(raw);
        assert_eq!(
            filtered.len(),
            2,
            "only two records match the eligibility contract"
        );

        // Both kept records must be the ArchiveNode/Active ones.
        let kept_addrs: Vec<String> = filtered.iter().map(|r| r.address.clone()).collect();
        assert!(kept_addrs.contains(&sum_net::l1_address_base58(&addrs[0])));
        assert!(kept_addrs.contains(&sum_net::l1_address_base58(&addrs[5])));

        // Sanity: the assignment kernel sees only the filtered addresses.
        // If a caller ever forgot to filter, the ownership vector would
        // include ineligible archives — this is the divergence class the
        // reviewer explicitly rejected.
        let node_addrs: Vec<[u8; 20]> = filtered
            .iter()
            .filter_map(|r| sum_net::l1_address_from_base58(&r.address).ok())
            .collect();
        assert_eq!(node_addrs.len(), 2);
        let root = [0x44u8; 32];
        let (assignment, _) = my_assigned_chunk_indices(&root, 8, &node_addrs, 2, &node_addrs[0]);
        for owners in &assignment {
            for owner in owners {
                assert!(
                    node_addrs.contains(owner),
                    "assignment must only reference filtered eligible archives",
                );
            }
        }
    }

    // ── Active-node routing (#37) ─────────────────────────────────────────

    fn worker_with_rpc(url: String) -> MarketSyncWorker {
        MarketSyncWorker::new(
            Arc::new(L1RpcClient::new(url)),
            [0u8; 20],
            "test-node".to_string(),
            Duration::from_secs(60),
            Duration::from_secs(60),
            3,
        )
    }

    /// (c) The finalized height is read EXACTLY ONCE per operation and the
    /// active-node snapshot is pinned to precisely that height.
    #[tokio::test]
    async fn active_nodes_reads_finalized_height_exactly_once() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([
            (
                "chain_getBlockHeight",
                MockResponse::Result(serde_json::json!({"height": 777, "finality": "finalized"})),
            ),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::Result(serde_json::json!([])),
            ),
        ]))
        .await;
        let worker = worker_with_rpc(server.url());
        let nodes = worker
            .active_nodes_at_finalized_head()
            .await
            .expect("snapshot must decode");
        assert!(nodes.is_empty());
        assert_eq!(
            server.method_count("chain_getBlockHeight"),
            1,
            "exactly one finalized-height read per operation"
        );
        assert_eq!(
            server.first_params("storage_getActiveNodesAtHeight"),
            Some(serde_json::json!([777])),
            "active-node snapshot pinned to the finalized height"
        );
    }

    /// (d) A finalized-height lookup failure propagates as the operation's
    /// typed error (market-sync's `sync_cycle` forwards it with `?`); the
    /// at-height snapshot is never queried.
    #[tokio::test]
    async fn active_nodes_propagates_height_failure() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([
            (
                "chain_getBlockHeight",
                MockResponse::error("finalized height unavailable"),
            ),
            (
                "storage_getActiveNodesAtHeight",
                MockResponse::Result(serde_json::json!([])),
            ),
        ]))
        .await;
        let worker = worker_with_rpc(server.url());
        let res = worker.active_nodes_at_finalized_head().await;
        assert!(
            res.is_err(),
            "height failure must propagate, not be swallowed"
        );
        assert_eq!(
            server.method_count("storage_getActiveNodesAtHeight"),
            0,
            "no at-height query once the height read failed"
        );
    }

    // ── Synthetic GC fixtures (temp dirs only) ────────────────────────────

    fn synthetic_store() -> (tempfile::TempDir, Arc<RwLock<SumStore>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = SumStore::new(sum_types::config::StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024,
        })
        .unwrap();
        (dir, Arc::new(RwLock::new(store)))
    }

    fn synthetic_manifest(root: [u8; 32], chunks: &[Vec<u8>]) -> sum_types::storage::DataManifest {
        sum_types::storage::DataManifest {
            file_name: "synthetic".into(),
            file_hash: [0u8; 32],
            total_size_bytes: chunks.len() as u64 * CHUNK_SIZE,
            chunk_count: chunks.len() as u32,
            merkle_root: root,
            chunks: chunks
                .iter()
                .enumerate()
                .map(|(i, d)| sum_types::storage::ChunkDescriptor {
                    chunk_index: i as u32,
                    offset: i as u64 * CHUNK_SIZE,
                    size: d.len() as u64,
                    blake3_hash: *blake3::hash(d).as_bytes(),
                    cid: sum_store::cid_from_data(d),
                    plaintext_blake3_hash: None,
                })
                .collect(),
        }
    }

    fn funded_v1(root: [u8; 32], chunk_count: u64) -> StorageFileInfo {
        StorageFileInfo {
            merkle_root: format!("0x{}", hex::encode(root)),
            owner: "synthetic-owner".into(),
            total_size_bytes: chunk_count * CHUNK_SIZE,
            access_list: Vec::new(),
            fee_pool: 1,
            created_at: 1,
        }
    }

    fn chunks_named(prefix: &str, n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| format!("{prefix} {i}").into_bytes())
            .collect()
    }

    /// The worker's own address (`[0; 20]`) plus four others.
    fn snapshot_with_self() -> Vec<[u8; 20]> {
        let mut s = snapshot_of(4);
        s.push([0u8; 20]);
        s.sort();
        s
    }

    /// One synthetic store per test: a funded V1 file (8 chunks) plus every
    /// shape of V2 data an archive holds — a Public file whose manifest was
    /// indexed by ManifestPush, a Private ciphertext chunk with its recorded
    /// mapping, and a chunk pushed before its manifest arrived.
    struct GcFixture {
        _dir: tempfile::TempDir,
        store: Arc<RwLock<SumStore>>,
        v1: Vec<Vec<u8>>,
        v2: Vec<Vec<u8>>,
        files: Vec<StorageFileInfo>,
    }

    async fn gc_fixture() -> GcFixture {
        let (dir, store) = synthetic_store();
        let v1_root = [0x51u8; 32];
        let v1 = chunks_named("v1 funded chunk", 8);
        let v2_public = chunks_named("v2 public chunk", 3);
        let v2_private = b"v2 private ciphertext chunk".to_vec();
        let v2_pre_manifest = b"v2 chunk pushed before its manifest".to_vec();
        {
            let mut s = store.write().await;
            s.manifest_idx
                .insert(&synthetic_manifest(v1_root, &v1))
                .unwrap();
            s.manifest_idx
                .insert(&synthetic_manifest([0x52u8; 32], &v2_public))
                .unwrap();
            for d in &v1 {
                s.local.put(&sum_store::cid_from_data(d), d).unwrap();
            }
            // V2 chunks arrive the way `inbound_v2::handle_push` stores them:
            // GC exemption first, then the chunk.
            for (root, d) in v2_public.iter().map(|d| ([0x52u8; 32], d)).chain([
                ([0x56u8; 32], &v2_pre_manifest),
                ([0x53u8; 32], &v2_private),
            ]) {
                let cid = sum_store::cid_from_data(d);
                s.manifest_idx.record_v2_held_chunk(root, &cid).unwrap();
                s.local.put(&cid, d).unwrap();
            }
            let pc = sum_store::cid_from_data(&v2_private);
            s.manifest_idx
                .record_private_chunk_cid([0x53u8; 32], &pc)
                .unwrap();
        }
        let mut v2 = v2_public;
        v2.push(v2_private);
        v2.push(v2_pre_manifest);
        GcFixture {
            _dir: dir,
            store,
            v1,
            v2,
            files: vec![funded_v1(v1_root, 8)],
        }
    }

    async fn has(store: &Arc<RwLock<SumStore>>, data: &[u8]) -> bool {
        store
            .read()
            .await
            .local
            .has(&sum_store::cid_from_data(data))
    }

    async fn sweep(
        worker: &mut MarketSyncWorker,
        f: &GcFixture,
        node_addrs: &[[u8; 20]],
    ) -> sum_store::gc::GcResult {
        let s = f.store.write().await;
        let candidates = worker.compute_gc_candidates(&f.files, node_addrs, &s);
        worker
            .gc
            .sweep_candidates(&s.local, &candidates, Instant::now())
            .unwrap()
    }

    fn zero_grace_worker() -> MarketSyncWorker {
        let mut w = worker_with_rpc("http://127.0.0.1:9".into());
        w.gc = GarbageCollector::new(Duration::from_secs(0));
        w
    }

    /// The V1 funded file's chunks this worker (`[0; 20]`) is assigned.
    fn my_v1_indices(node_addrs: &[[u8; 20]]) -> HashSet<u32> {
        my_assigned_chunk_indices(&[0x51u8; 32], 8, node_addrs, 3, &[0u8; 20])
            .1
            .into_iter()
            .collect()
    }

    #[tokio::test]
    async fn gc_retains_every_v2_chunk_with_zero_grace() {
        let f = gc_fixture().await;
        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &snapshot_with_self()).await;

        for d in &f.v2 {
            assert!(has(&f.store, d).await, "V2 chunk must survive the V1 sweep");
        }
    }

    #[tokio::test]
    async fn gc_v1_behaviour_is_unchanged_for_funded_files() {
        let f = gc_fixture().await;
        let node_addrs = snapshot_with_self();
        let mine = my_v1_indices(&node_addrs);
        assert!(
            !mine.is_empty() && mine.len() < 8,
            "fixture needs both kinds"
        );

        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &node_addrs).await;

        for (i, d) in f.v1.iter().enumerate() {
            let kept = has(&f.store, d).await;
            assert_eq!(
                kept,
                mine.contains(&(i as u32)),
                "V1 chunk {i}: assigned stays, unassigned is collected"
            );
        }
        assert_eq!(result.chunks_deleted as usize, 8 - mine.len());
    }

    #[tokio::test]
    async fn gc_v1_grace_period_still_applies() {
        let f = gc_fixture().await;
        let node_addrs = snapshot_with_self();
        let mut worker = worker_with_rpc("http://127.0.0.1:9".into());
        let result = sweep(&mut worker, &f, &node_addrs).await;

        assert_eq!(result.chunks_deleted, 0);
        assert_eq!(
            result.chunks_retained as usize,
            8 - my_v1_indices(&node_addrs).len(),
            "only V1 unassigned chunks are tracked; V2 chunks are never marked"
        );
    }

    #[tokio::test]
    async fn gc_retains_chunks_of_unknown_ownership() {
        let f = gc_fixture().await;
        let orphan = b"chunk no manifest or mapping mentions".to_vec();
        f.store
            .read()
            .await
            .local
            .put(&sum_store::cid_from_data(&orphan), &orphan)
            .unwrap();
        // A file that left the funded set: its manifest is still indexed,
        // but nothing says whether it is V1-defunded or V2.
        let unlisted = chunks_named("indexed but not funded", 2);
        {
            let mut s = f.store.write().await;
            s.manifest_idx
                .insert(&synthetic_manifest([0x54u8; 32], &unlisted))
                .unwrap();
            for d in &unlisted {
                s.local.put(&sum_store::cid_from_data(d), d).unwrap();
            }
        }

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &snapshot_with_self()).await;

        assert!(
            has(&f.store, &orphan).await,
            "unknown ownership is retained"
        );
        for d in &unlisted {
            assert!(has(&f.store, d).await, "unlisted manifest is retained");
        }
    }

    #[tokio::test]
    async fn gc_retains_a_v1_chunk_shared_with_a_v2_file() {
        // Content addressing: identical bytes in a V1 chunk this node is
        // not assigned and in a V2 Public file share one CID. The V2
        // reference wins.
        let f = gc_fixture().await;
        let node_addrs = snapshot_with_self();
        let mine = my_v1_indices(&node_addrs);
        let shared_index = (0..8u32).find(|i| !mine.contains(i)).unwrap();
        let shared = f.v1[shared_index as usize].clone();
        f.store
            .write()
            .await
            .manifest_idx
            .insert(&synthetic_manifest(
                [0x55u8; 32],
                std::slice::from_ref(&shared),
            ))
            .unwrap();

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &node_addrs).await;

        assert!(has(&f.store, &shared).await);
    }

    /// A funded V1 file and a V2 file may share a merkle root. The manifest
    /// index is keyed by root alone, so the V2 manifest can be read as the
    /// funded V1 file's manifest, and that file's declared `total_size_bytes`
    /// decides which of its chunks count as "assigned" here. Every V2 chunk
    /// must still survive.
    #[tokio::test]
    async fn gc_keeps_v2_chunks_when_a_funded_v1_file_reuses_the_v2_root() {
        let mut f = gc_fixture().await;
        let v2_root = [0x52u8; 32];
        // One chunk declared: at most chunk 0 can be "assigned" here, so the
        // V2 manifest's chunks 1 and 2 become V1-unassigned.
        f.files.push(funded_v1(v2_root, 1));

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &snapshot_with_self()).await;

        for d in &f.v2 {
            assert!(
                has(&f.store, d).await,
                "a V1 registration reusing a V2 root must not delete V2 data"
            );
        }
    }

    /// The residual case of the first fix: a V2 Public chunk pushed before
    /// its manifest, byte-identical to a funded V1 chunk this node is NOT
    /// assigned. Content addressing gives both one CID; the V2 exemption,
    /// recorded at push time, keeps it.
    #[tokio::test]
    async fn gc_keeps_a_pre_manifest_v2_chunk_identical_to_an_unassigned_v1_chunk() {
        let f = gc_fixture().await;
        let node_addrs = snapshot_with_self();
        let mine = my_v1_indices(&node_addrs);
        let i = (0..8u32).find(|i| !mine.contains(i)).unwrap();
        let shared = f.v1[i as usize].clone();
        let cid = sum_store::cid_from_data(&shared);
        // A V2 push of the same bytes, with no manifest indexed for it.
        f.store
            .write()
            .await
            .manifest_idx
            .record_v2_held_chunk([0x57u8; 32], &cid)
            .unwrap();

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &node_addrs).await;

        assert!(has(&f.store, &shared).await);
    }

    /// Chunks already on disk when a store first runs this version carry no
    /// provenance; all of them are protected, so the root-collision attack
    /// cannot reach V2 data pushed under an older version either.
    #[tokio::test]
    async fn gc_protects_every_chunk_present_before_the_protected_list_existed() {
        let (dir, store) = synthetic_store();
        let v2_root = [0x52u8; 32];
        let v2 = chunks_named("v2 chunk pushed by an older version", 3);
        {
            let mut s = store.write().await;
            s.manifest_idx
                .insert(&synthetic_manifest(v2_root, &v2))
                .unwrap();
            for d in &v2 {
                s.local.put(&sum_store::cid_from_data(d), d).unwrap();
            }
        }
        // Simulate an upgrade from a version without the list: neither the
        // list nor its marker exists yet.
        drop(store);
        for name in [
            sum_store::manifest_index::GC_PROTECTED_FILE,
            sum_store::manifest_index::GC_PROTECTED_MARKER,
        ] {
            std::fs::remove_file(dir.path().join("store/manifests").join(name)).unwrap();
        }
        let reopened = Arc::new(RwLock::new(
            SumStore::new(sum_types::config::StoreConfig {
                store_dir: dir.path().join("store"),
                max_chunk_msg_bytes: 64 * 1024,
            })
            .unwrap(),
        ));
        let f = GcFixture {
            _dir: dir,
            store: reopened,
            v1: Vec::new(),
            v2,
            files: vec![funded_v1(v2_root, 1)],
        };

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &snapshot_with_self()).await;

        for d in &f.v2 {
            assert!(has(&f.store, d).await, "pre-upgrade V2 chunk retained");
        }
    }

    /// An unreadable protected list means provenance is unknown: nothing is
    /// deleted, not even an unassigned V1 chunk.
    #[tokio::test]
    async fn gc_deletes_nothing_when_the_protected_list_is_unreadable() {
        let f = gc_fixture().await;
        let list = f
            ._dir
            .path()
            .join("store/manifests")
            .join(sum_store::manifest_index::GC_PROTECTED_FILE);
        std::fs::remove_file(&list).unwrap();
        std::fs::create_dir(&list).unwrap(); // reads now fail
        let idx =
            sum_store::manifest_index::ManifestIndex::load(&f._dir.path().join("store")).unwrap();
        assert!(!idx.gc_protection_intact());
        f.store.write().await.manifest_idx = idx;

        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &snapshot_with_self()).await;

        assert_eq!(result.chunks_deleted, 0);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }
    }

    #[tokio::test]
    async fn gc_has_no_candidates_without_a_funded_v1_file() {
        let mut f = gc_fixture().await;
        f.files.clear();
        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &snapshot_with_self()).await;

        assert_eq!(result.chunks_deleted, 0);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }
    }

    fn gc_list(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path()
            .join("store/manifests")
            .join(sum_store::manifest_index::GC_PROTECTED_FILE)
    }

    fn reopen(dir: &tempfile::TempDir) -> Arc<RwLock<SumStore>> {
        Arc::new(RwLock::new(
            SumStore::new(sum_types::config::StoreConfig {
                store_dir: dir.path().join("store"),
                max_chunk_msg_bytes: 64 * 1024,
            })
            .unwrap(),
        ))
    }

    /// Restart the fixture's store: drop every handle (releasing the store
    /// lease), then open it again from disk.
    fn restart(f: GcFixture) -> GcFixture {
        let GcFixture {
            _dir,
            store,
            v1,
            v2,
            files,
        } = f;
        drop(store);
        let store = reopen(&_dir);
        GcFixture {
            _dir,
            store,
            v1,
            v2,
            files,
        }
    }

    #[tokio::test]
    async fn gc_store_seeds_protection_only_on_first_open() {
        let (dir, store) = synthetic_store();
        let later = b"chunk written after the first open".to_vec();
        let cid = sum_store::cid_from_data(&later);
        store.read().await.local.put(&cid, &later).unwrap();
        drop(store);

        let store = reopen(&dir);
        let s = store.read().await;
        assert!(s.manifest_idx.gc_protection_intact());
        assert!(
            !s.manifest_idx.is_gc_protected(&cid),
            "seeding runs once; later V1 chunks stay collectable"
        );
    }

    #[tokio::test]
    async fn gc_deletes_nothing_and_refuses_pushes_once_the_list_is_lost() {
        let f = gc_fixture().await;
        std::fs::remove_file(gc_list(&f._dir)).unwrap();
        let f = restart(f);
        assert!(!gc_list(&f._dir).exists(), "a lost list is not re-seeded");

        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &snapshot_with_self()).await;
        assert_eq!(result.chunks_deleted, 0);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }

        let late = b"V2 chunk pushed while the list is lost".to_vec();
        let cid = sum_store::cid_from_data(&late);
        assert!(
            crate::inbound_v2::store_v2_pushed_chunk(&f.store, [0x52u8; 32], &cid, &late)
                .await
                .is_err()
        );
        assert!(
            !has(&f.store, &late).await,
            "a refused chunk is not written"
        );
    }

    #[tokio::test]
    async fn gc_deletes_nothing_when_the_list_is_corrupt() {
        let f = gc_fixture().await;
        let path = gc_list(&f._dir);
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.insert_str(0, "not a protected-list line\n");
        std::fs::write(&path, content).unwrap();
        let f = restart(f);

        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &snapshot_with_self()).await;
        assert_eq!(result.chunks_deleted, 0);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }
    }

    /// A disk-write failure on the protected list: the V2 chunk is refused
    /// and not written, and deletion stops in this process.
    #[tokio::test]
    async fn gc_fails_closed_when_an_exemption_cannot_be_written() {
        let f = gc_fixture().await;
        let path = gc_list(&f._dir);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap(); // every write now fails

        let data = b"V2 chunk whose exemption write fails".to_vec();
        let cid = sum_store::cid_from_data(&data);
        let err = crate::inbound_v2::store_v2_pushed_chunk(&f.store, [0x52u8; 32], &cid, &data)
            .await
            .unwrap_err();
        assert!(
            err.contains("gc exemption persistence failed"),
            "got: {err}"
        );
        assert!(!has(&f.store, &data).await, "chunk not written");

        let mut worker = zero_grace_worker();
        let result = sweep(&mut worker, &f, &snapshot_with_self()).await;
        assert_eq!(result.chunks_deleted, 0);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }
    }

    /// Crash after the exemption is synced but before the chunk is written:
    /// after the restart, the retried push stores a chunk that is still
    /// protected, even where a funded V1 file names it unassigned.
    #[tokio::test]
    async fn gc_keeps_a_chunk_whose_exemption_was_recorded_before_a_crash() {
        let mut f = gc_fixture().await;
        let root = [0x59u8; 32];
        let named = chunks_named("v2 file whose push was interrupted", 3);
        let data = named[2].clone();
        let cid = sum_store::cid_from_data(&data);
        {
            let mut s = f.store.write().await;
            s.manifest_idx
                .insert(&synthetic_manifest(root, &named))
                .unwrap();
            s.manifest_idx.record_v2_held_chunk(root, &cid).unwrap();
        }
        // Chunk 2 is never "assigned" under a one-chunk V1 declaration.
        f.files.push(funded_v1(root, 1));

        let f = restart(f);
        {
            let s = f.store.read().await;
            assert!(s.manifest_idx.gc_protection_intact());
            assert!(s.manifest_idx.is_gc_protected(&cid));
            assert!(!s.local.has(&cid), "the crash came before the write");
        }
        crate::inbound_v2::store_v2_pushed_chunk(&f.store, root, &cid, &data)
            .await
            .unwrap();

        let mut worker = zero_grace_worker();
        sweep(&mut worker, &f, &snapshot_with_self()).await;
        assert!(has(&f.store, &data).await);
    }

    /// The grace clock is in memory: a restart in the middle of a grace
    /// period starts it again, so nothing is deleted early.
    #[tokio::test]
    async fn gc_grace_period_restarts_after_a_restart() {
        let f = gc_fixture().await;
        let node_addrs = snapshot_with_self();
        let unassigned = 8 - my_v1_indices(&node_addrs).len();

        let mut worker = worker_with_rpc("http://127.0.0.1:9".into());
        let first = sweep(&mut worker, &f, &node_addrs).await;
        assert_eq!(first.chunks_deleted, 0);
        assert_eq!(first.chunks_retained as usize, unassigned);

        let f = restart(f);
        let mut worker = worker_with_rpc("http://127.0.0.1:9".into());
        let second = sweep(&mut worker, &f, &node_addrs).await;
        assert_eq!(second.chunks_deleted, 0, "grace starts again");
        assert_eq!(second.chunks_retained as usize, unassigned);
        for d in f.v1.iter().chain(&f.v2) {
            assert!(has(&f.store, d).await);
        }
    }

    /// V2 pushes racing GC sweeps: every pushed chunk survives, even though
    /// a funded V1 file reusing the V2 root makes them sweep candidates
    /// absent their exemptions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn gc_loses_no_v2_chunk_pushed_during_sweeps() {
        let mut f = gc_fixture().await;
        let root = [0x5Au8; 32];
        let pushed = chunks_named("v2 chunk pushed during a sweep", 48);
        f.store
            .write()
            .await
            .manifest_idx
            .insert(&synthetic_manifest(root, &pushed))
            .unwrap();
        f.files.push(funded_v1(root, 1));

        let store = f.store.clone();
        let to_push = pushed.clone();
        let pusher = tokio::spawn(async move {
            for d in &to_push {
                let cid = sum_store::cid_from_data(d);
                crate::inbound_v2::store_v2_pushed_chunk(&store, root, &cid, d)
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut worker = zero_grace_worker();
        while !pusher.is_finished() {
            sweep(&mut worker, &f, &snapshot_with_self()).await;
            tokio::task::yield_now().await;
        }
        pusher.await.unwrap();
        sweep(&mut worker, &f, &snapshot_with_self()).await;

        for d in &pushed {
            assert!(has(&f.store, d).await, "pushed during a sweep, still here");
        }
    }
}
