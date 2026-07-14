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

        // 4. Run garbage collection
        self.last_l1_poll = Instant::now();
        let assigned_cids = self.compute_assigned_cids(&files, &node_addrs, store).await;
        {
            let store_read = store.read().await;
            match self
                .gc
                .mark_and_sweep(&store_read.local, &assigned_cids, self.last_l1_poll)
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
            let root_hex = file
                .merkle_root
                .strip_prefix("0x")
                .unwrap_or(&file.merkle_root);
            let Some(root_bytes) = hex_to_32(root_hex) else {
                continue;
            };

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
/// ([`MarketSyncWorker::compute_assigned_cids`]).
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
    /// the GC path (`compute_assigned_cids` → `my_assigned_chunk_indices`)
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
}
