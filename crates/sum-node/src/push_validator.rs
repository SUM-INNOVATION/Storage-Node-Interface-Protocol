//! V2 push validator (chain plan v3.2 §3.6 receive-side).
//!
//! Each `ShardRequestV2::Push { data, merkle_root, chunk_index, merkle_path }`
//! we receive must clear three independent chain-rooted gates before
//! the chunk enters the local store. None of these are derivable from
//! the push payload alone — all three need a chain-state read:
//!
//!   1. **File exists and is not Abandoned.** Look up
//!      `storage_getFileInfoV2(root)`; require lifecycle ∈ {Pending, Active}.
//!   2. **Our archive is assigned this chunk.** Read the immutable
//!      snapshot at `info.assignment_height` via
//!      `storage_getActiveNodesAtHeight`; the V2 rendezvous-hash
//!      (`assignment_v2`) must place us in the top-R for this
//!      `chunk_index`.
//!   3. **Merkle proof verifies.** Use the strict
//!      [`verify_merkle_proof_bytes_for_tree`] variant against the
//!      chain-registered `chunk_count` so a peer can't lie about both
//!      `chunk_count` and `chunk_index` to forge a shape-bypassing
//!      proof.
//!
//! The validator returns the BLAKE3 leaf hash on success — call sites
//! derive the CID via [`sum_store::content_id::cid_from_blake3_hash`]
//! and store the chunk under that CID. The wire format intentionally
//! does not carry a CID; the leaf hash is derivable from the data and
//! the content-address comes from there.
//!
//! ## Caching
//!
//! Two LRU caches sit in front of the RPC:
//!   * `file_info` keyed by `[u8; 32]` wire merkle_root → `StorageFileInfoV2`.
//!   * `snapshot`  keyed by `u64` assignment_height → canonical
//!     `Vec<[u8; 20]>` (decoded from base58, deduped, sorted ascending —
//!     the exact preprocessing `assignment_v2` re-applies internally).
//!
//! Both fills are racy under concurrent first-touches: two callers may
//! both miss, both RPC, both write back. That's deliberate — chain
//! responses are deterministic for a given key, so the second writer's
//! identical replacement is a no-op. We avoid `tokio::sync::Mutex`
//! around the whole validate call so independent merkle_roots don't
//! serialize on each other.
//!
//! Snapshots are immutable per height (chain plan §5.3 Ask 15 Option A),
//! so the snapshot cache never goes stale. File info CAN go stale — a
//! Pending file may be activated or abandoned while we hold the cache —
//! which has two consequences:
//!   * **Abandoned-while-cached-Active**: we accept the chunk and store
//!     it. The downstream `AcceptAssignmentV2` attestation will still
//!     submit; chain rejects it (file is Abandoned) and the popcount
//!     never crosses `ActivateFileV2`. The chunk on disk is wasted but
//!     not unsafe. Acceptable for Phase 0b.
//!   * **Pending-while-cached-Abandoned**: not possible (Abandoned is
//!     terminal per §3.5).
//!
//! W6 `AssignmentAttestor` and W10 `run_ingest` will tighten the TTL
//! when they orchestrate full file lifecycles; the Phase 0b receive-side
//! position is "chain is the journal, our cache is best-effort."
//!
//! ## Hot-path performance
//!
//! Per-push, validation does:
//!   * 0–1 file_info RPC (cached on miss)
//!   * 0–1 snapshot RPC (cached on miss)
//!   * `O(snapshot_size)` BLAKE3 hashes for `assigned_archives` —
//!     it scores every candidate in the snapshot (typical archive
//!     count: tens), then sorts and takes the top R. Not `O(R)`.
//!   * `proof_depth(chunk_count)` BLAKE3 hashes for the Merkle proof
//!
//! Crucially we use `assigned_archives` (per-`chunk_index`,
//! `O(snapshot_size)`) — NOT `chunks_for_archive_v2` which walks the
//! whole `[0, chunk_count)` range, costing
//! `O(chunk_count × snapshot_size)`. The latter is the right call for
//! the W6 attestor (one walk yields the whole assigned set); here it
//! would turn one push into ~100M hashes for a 1M-chunk file.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use anyhow::Context;
use lru::LruCache;
use sum_net::l1_address_from_base58;
use sum_store::assignment_v2::assigned_archives;
use sum_store::verify::verify_merkle_proof_bytes_for_tree;
use sum_types::rpc_types::{NodeRecordInfo, StorageFileInfoV2};

/// Chain-plan-v3.2 V2 parameters consumed by the push validator AND
/// the assignment attestor.
///
/// Hardcoded today; chain team has `chain_getChainParams` on their
/// backlog. When that lands, swap the source — call sites take a
/// `&V2Params` so no logic changes.
#[derive(Debug, Clone, Copy)]
pub struct V2Params {
    /// `R` in the V2 rendezvous-hash: number of archives assigned to
    /// each chunk. Chain plan §3.4 default = 3.
    pub assignment_replication_factor: u32,
    /// Maximum `chunk_indices.len()` accepted in a single
    /// `AcceptAssignmentV2` transaction. Files whose per-archive
    /// assignment exceeds this cap split across multiple txs that
    /// OR-merge into the same per-(file, archive) bitmap on chain.
    /// Chain plan §3.6 / §3.4 default = 65,536.
    pub max_chunk_indices_per_tx: u32,
}

impl Default for V2Params {
    fn default() -> Self {
        Self::DEFAULTS
    }
}

impl V2Params {
    pub const DEFAULTS: V2Params = V2Params {
        assignment_replication_factor: 3,
        max_chunk_indices_per_tx: 65_536,
    };
}

/// Subset of the V2 RPC client the validator depends on. Defined as a
/// trait so tests can mock without spinning up an HTTP responder.
#[async_trait::async_trait]
pub trait V2RpcClient: Send + Sync {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> anyhow::Result<StorageFileInfoV2>;

    async fn storage_get_active_nodes_at_height(
        &self,
        height: u64,
    ) -> anyhow::Result<Vec<NodeRecordInfo>>;
}

/// Reasons a push gets rejected. Each variant maps to a distinct
/// Successful validation outcome. Carries the chunk's BLAKE3 leaf
/// hash (used by the dispatcher to derive the on-disk CID) and the
/// chain-stated visibility so downstream paths don't have to re-probe
/// the chain to know whether this is a Public or Private file —
/// `validate_push` already had to fetch the file info to verify the
/// Merkle proof, and that fetch is the canonical source of truth.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedPush {
    pub leaf_hash: [u8; 32],
    pub visibility: sum_types::rpc_types::VisibilityV2,
}

/// receive-side response so the sender can reason about whether to
/// retry. Phase 0b doesn't yet emit per-reason error codes on the wire
/// (W5 will); locally we use this enum as the call-site discriminant.
#[derive(Debug, thiserror::Error)]
pub enum PushReject {
    /// `storage_getFileInfoV2(root)` returned an RPC error — typically
    /// "file not registered" but could also be a transport failure.
    /// Either way: do not store the chunk. Caller may retry with
    /// backoff; chain registration may still be in-flight on the owner
    /// side.
    #[error("file_info lookup failed for root: {0}")]
    UnknownRoot(#[source] anyhow::Error),

    /// File is in lifecycle Abandoned. Push must be rejected even if
    /// the proof is valid; downstream attestation would be dropped on
    /// chain anyway.
    #[error("file is Abandoned")]
    Abandoned,

    /// `chunk_index >= chunk_count` for the registered file. Caught
    /// before any snapshot lookup — this is a fast-fail guard against
    /// peers fabricating proofs against shapes the chain doesn't
    /// recognize.
    #[error("chunk_index {chunk_index} >= chunk_count {chunk_count}")]
    OutOfRange { chunk_index: u32, chunk_count: u32 },

    /// `storage_getActiveNodesAtHeight(h)` returned an RPC error.
    #[error("snapshot lookup failed at height {height}: {source}")]
    SnapshotLookup {
        height: u64,
        #[source]
        source: anyhow::Error,
    },

    /// Snapshot at the file's assignment_height does not contain our
    /// archive's address. Either we registered after this snapshot, or
    /// the chain returned a snapshot we shouldn't be participating in.
    /// Reject — we're not a valid storage target for this file.
    #[error("our archive (addr={my_addr_hex}) is not in the snapshot at height {height}")]
    NotInSnapshot { height: u64, my_addr_hex: String },

    /// Our archive IS in the snapshot but the V2 rendezvous-hash does
    /// NOT place us in the top-R for `chunk_index`. The owner is
    /// pushing to the wrong archive; reject without storing.
    #[error("chunk_index {chunk_index} not assigned to this archive for this file")]
    NotAssigned { chunk_index: u32 },

    /// Chain response was structurally malformed — addresses that
    /// don't decode from base58, merkle_root that isn't a 0x + 64 hex
    /// string, etc. This is always a bug or chain misconfiguration,
    /// not a peer-misbehavior signal.
    #[error("malformed chain response: {0}")]
    BadChainShape(String),

    /// Strict Merkle proof failed verification against
    /// `(merkle_root, chunk_index, chunk_count)` from chain state.
    /// This is the receive-side gate the chain executor and SNIP both
    /// enforce; rejection here matches what the chain's PoR challenge
    /// would also catch.
    #[error("Merkle proof failed verification")]
    BadProof,
}

/// Default LRU capacities. Both caches are small — Phase 0b ingests are
/// one-file-at-a-time, so the working set is tiny. The values are
/// generous enough to absorb a few concurrent files without thrashing.
pub const DEFAULT_FILE_INFO_CAP: usize = 256;
pub const DEFAULT_SNAPSHOT_CAP: usize = 64;

pub struct PushValidator<C: V2RpcClient> {
    rpc: C,
    my_addr: [u8; 20],
    params: V2Params,
    file_info: Mutex<LruCache<[u8; 32], StorageFileInfoV2>>,
    snapshot: Mutex<LruCache<u64, Vec<[u8; 20]>>>,
}

impl<C: V2RpcClient> PushValidator<C> {
    pub fn new(rpc: C, my_addr: [u8; 20], params: V2Params) -> Self {
        Self::with_capacities(
            rpc,
            my_addr,
            params,
            NonZeroUsize::new(DEFAULT_FILE_INFO_CAP).unwrap(),
            NonZeroUsize::new(DEFAULT_SNAPSHOT_CAP).unwrap(),
        )
    }

    pub fn with_capacities(
        rpc: C,
        my_addr: [u8; 20],
        params: V2Params,
        file_info_cap: NonZeroUsize,
        snapshot_cap: NonZeroUsize,
    ) -> Self {
        Self {
            rpc,
            my_addr,
            params,
            file_info: Mutex::new(LruCache::new(file_info_cap)),
            snapshot: Mutex::new(LruCache::new(snapshot_cap)),
        }
    }

    /// Validate one inbound push. Returns the BLAKE3 leaf hash of
    /// `data` on success — call sites convert to a CID via
    /// [`sum_store::content_id::cid_from_blake3_hash`] for storage.
    ///
    /// `merkle_root`, `chunk_index`, and `merkle_path` come straight
    /// off the wire; `data` is the chunk bytes.
    pub async fn validate_push(
        &self,
        merkle_root: [u8; 32],
        chunk_index: u32,
        data: &[u8],
        merkle_path: &[[u8; 32]],
    ) -> Result<ValidatedPush, PushReject> {
        let leaf_hash: [u8; 32] = *blake3::hash(data).as_bytes();

        let info = self.fetch_file_info(&merkle_root).await?;

        // Lifecycle gate: Phase 0b accepts Pending or Active; Abandoned
        // is terminal-no-attest per chain plan §3.5. Future lifecycle
        // states (e.g. Rotated from Ask 10) would need to be classified
        // explicitly when chain plan v3.3 lands.
        if info.lifecycle.is_abandoned() {
            return Err(PushReject::Abandoned);
        }
        if !(info.lifecycle.is_pending() || info.lifecycle.is_active()) {
            return Err(PushReject::BadChainShape(format!(
                "unexpected lifecycle byte {} for root 0x{}",
                info.lifecycle.0,
                hex::encode(merkle_root),
            )));
        }

        // Bounds check BEFORE snapshot lookup — cheap fast-fail, and a
        // legitimate prerequisite for the strict Merkle verifier
        // anyway. Means a peer fabricating chunk_index = u32::MAX never
        // forces us to fetch a snapshot.
        if chunk_index >= info.chunk_count {
            return Err(PushReject::OutOfRange {
                chunk_index,
                chunk_count: info.chunk_count,
            });
        }

        let snapshot = self.fetch_snapshot(info.assignment_height).await?;

        if snapshot.binary_search(&self.my_addr).is_err() {
            return Err(PushReject::NotInSnapshot {
                height: info.assignment_height,
                my_addr_hex: hex::encode(self.my_addr),
            });
        }

        let assigned = assigned_archives(
            &merkle_root,
            &snapshot,
            chunk_index,
            self.params.assignment_replication_factor,
        );
        if !assigned.iter().any(|a| a == &self.my_addr) {
            return Err(PushReject::NotAssigned { chunk_index });
        }

        // Strict Merkle gate. `total_leaves` must come from chain state,
        // never the wire — that's the whole point of `verify_*_for_tree`.
        if !verify_merkle_proof_bytes_for_tree(
            &leaf_hash,
            chunk_index,
            merkle_path,
            &merkle_root,
            info.chunk_count,
        ) {
            return Err(PushReject::BadProof);
        }

        // Visibility comes from the same `info` we already validated
        // above. Returning it here means downstream callers (the V2
        // dispatcher) don't need a second probe to know whether to
        // record the Phase 4b ciphertext-CID → root mapping.
        Ok(ValidatedPush {
            leaf_hash,
            visibility: info.visibility,
        })
    }

    /// Cache-fronted wrapper around `storage_getFileInfoV2`. On miss,
    /// performs the RPC, decodes the chain's `merkle_root_hex` field
    /// to verify it round-trips to the wire bytes we asked about, and
    /// inserts on success.
    async fn fetch_file_info(
        &self,
        merkle_root: &[u8; 32],
    ) -> Result<StorageFileInfoV2, PushReject> {
        if let Some(info) = self
            .file_info
            .lock()
            .expect("file_info mutex poisoned")
            .get(merkle_root)
            .cloned()
        {
            return Ok(info);
        }

        let root_hex = format!("0x{}", hex::encode(merkle_root));
        let info = self
            .rpc
            .storage_get_file_info_v2(&root_hex)
            .await
            .map_err(PushReject::UnknownRoot)?;

        // Chain self-consistency: the `merkle_root` field in the
        // response MUST equal what we asked about. A mismatch is a
        // chain bug or RPC misroute; treat as bad shape rather than
        // accepting the row.
        let returned = decode_root_hex(&info.merkle_root)
            .map_err(|e| PushReject::BadChainShape(e.to_string()))?;
        if &returned != merkle_root {
            return Err(PushReject::BadChainShape(format!(
                "asked for root 0x{} but chain returned root {}",
                hex::encode(merkle_root),
                info.merkle_root,
            )));
        }

        self.file_info
            .lock()
            .expect("file_info mutex poisoned")
            .put(*merkle_root, info.clone());
        Ok(info)
    }

    /// Cache-fronted wrapper around `storage_getActiveNodesAtHeight`.
    /// On miss, performs the RPC, decodes every node address from
    /// base58, canonicalizes to (sorted, deduped) `Vec<[u8; 20]>` so
    /// `assignment_v2` doesn't redo the work per push.
    async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>, PushReject> {
        if let Some(snap) = self
            .snapshot
            .lock()
            .expect("snapshot mutex poisoned")
            .get(&height)
            .cloned()
        {
            return Ok(snap);
        }

        let raw = self
            .rpc
            .storage_get_active_nodes_at_height(height)
            .await
            .map_err(|e| PushReject::SnapshotLookup { height, source: e })?;

        let mut decoded: Vec<[u8; 20]> = Vec::with_capacity(raw.len());
        for record in &raw {
            let addr = l1_address_from_base58(&record.address).map_err(|e| {
                PushReject::BadChainShape(format!(
                    "snapshot at h={height} contained unparseable address {:?}: {e}",
                    record.address
                ))
            })?;
            decoded.push(addr);
        }
        decoded.sort();
        decoded.dedup();

        self.snapshot
            .lock()
            .expect("snapshot mutex poisoned")
            .put(height, decoded.clone());
        Ok(decoded)
    }
}

fn decode_root_hex(hex_str: &str) -> anyhow::Result<[u8; 32]> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(stripped).context("merkle_root not hex-decodable")?;
    if bytes.len() != 32 {
        anyhow::bail!("merkle_root expected 32 bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use sum_net::l1_address_base58;
    use sum_store::merkle::MerkleTree;
    use sum_types::rpc_types::{LifecycleV2, VisibilityV2};

    /// In-memory mock implementing the V2 RPC subset. Configured with
    /// canned responses keyed by (root_hex / height) and records each
    /// call so tests can assert cache behavior.
    #[derive(Default)]
    struct MockRpc {
        file_infos: StdMutex<HashMap<String, StorageFileInfoV2>>,
        snapshots: StdMutex<HashMap<u64, Vec<NodeRecordInfo>>>,
        file_info_calls: StdMutex<Vec<String>>,
        snapshot_calls: StdMutex<Vec<u64>>,
    }

    impl MockRpc {
        fn add_file(&self, root_hex: &str, info: StorageFileInfoV2) {
            self.file_infos
                .lock()
                .unwrap()
                .insert(root_hex.to_string(), info);
        }
        fn add_snapshot(&self, height: u64, nodes: Vec<NodeRecordInfo>) {
            self.snapshots.lock().unwrap().insert(height, nodes);
        }
        fn file_info_call_count(&self) -> usize {
            self.file_info_calls.lock().unwrap().len()
        }
        fn snapshot_call_count(&self) -> usize {
            self.snapshot_calls.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl V2RpcClient for MockRpc {
        async fn storage_get_file_info_v2(
            &self,
            merkle_root_hex: &str,
        ) -> anyhow::Result<StorageFileInfoV2> {
            self.file_info_calls
                .lock()
                .unwrap()
                .push(merkle_root_hex.to_string());
            self.file_infos
                .lock()
                .unwrap()
                .get(merkle_root_hex)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown root: {merkle_root_hex}"))
        }

        async fn storage_get_active_nodes_at_height(
            &self,
            height: u64,
        ) -> anyhow::Result<Vec<NodeRecordInfo>> {
            self.snapshot_calls.lock().unwrap().push(height);
            self.snapshots
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no snapshot at height {height}"))
        }
    }

    fn node_record(addr: &[u8; 20]) -> NodeRecordInfo {
        NodeRecordInfo {
            address: l1_address_base58(addr),
            role: "ArchiveNode".into(),
            staked_balance: 1_000_000_000,
            status: "Active".into(),
            registered_at: 1,
        }
    }

    /// Build a real Merkle tree over `n` chunks; return the root,
    /// chunk_count, and a closure that yields `(data, proof)` for any
    /// chunk index.
    struct TreeFixture {
        root: [u8; 32],
        chunk_count: u32,
        tree: MerkleTree,
        chunks: Vec<Vec<u8>>,
    }

    impl TreeFixture {
        fn new(n: u32, salt: u8) -> Self {
            let chunks: Vec<Vec<u8>> = (0..n)
                .map(|i| {
                    let mut v = vec![salt; 64];
                    v.extend_from_slice(&i.to_le_bytes());
                    v
                })
                .collect();
            let leaves: Vec<blake3::Hash> =
                chunks.iter().map(|c| blake3::hash(c)).collect();
            let tree = MerkleTree::build(&leaves);
            let root: [u8; 32] = *tree.root().as_bytes();
            Self {
                root,
                chunk_count: n,
                tree,
                chunks,
            }
        }
        fn proof(&self, chunk_index: u32) -> Vec<[u8; 32]> {
            self.tree.proof_bytes(chunk_index)
        }
        fn data(&self, chunk_index: u32) -> &[u8] {
            &self.chunks[chunk_index as usize]
        }
    }

    /// Find a chunk_index for which `assigned_archives` includes
    /// `target` — exhausts `[0, chunk_count)` and panics if none
    /// exists. Used to set up tests that need a known-good chunk for a
    /// specific archive.
    fn first_chunk_assigned_to(
        root: &[u8; 32],
        snapshot: &[[u8; 20]],
        chunk_count: u32,
        target: &[u8; 20],
        r: u32,
    ) -> u32 {
        for i in 0..chunk_count {
            let assigned = assigned_archives(root, snapshot, i, r);
            if assigned.iter().any(|a| a == target) {
                return i;
            }
        }
        panic!(
            "no chunk in [0, {chunk_count}) assigned to target — \
             snapshot or chunk_count too small for tests"
        );
    }

    /// Find a chunk_index NOT assigned to `target`. Useful for the
    /// NotAssigned reject path. Panics if every chunk is assigned to
    /// the target (i.e. snapshot.len() <= R).
    fn first_chunk_not_assigned_to(
        root: &[u8; 32],
        snapshot: &[[u8; 20]],
        chunk_count: u32,
        target: &[u8; 20],
        r: u32,
    ) -> u32 {
        for i in 0..chunk_count {
            let assigned = assigned_archives(root, snapshot, i, r);
            if !assigned.iter().any(|a| a == target) {
                return i;
            }
        }
        panic!(
            "every chunk in [0, {chunk_count}) was assigned to target — \
             snapshot.len() <= R; can't construct a NotAssigned test"
        );
    }

    /// Build a 5-archive snapshot at addresses 0xAA..00..00, 0xAB..,
    /// etc. so we have a stable ordering and can pick "my_addr" from
    /// the middle.
    fn five_archives() -> Vec<[u8; 20]> {
        (0..5)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xAA + i;
                a
            })
            .collect()
    }

    fn file_info(
        root: &[u8; 32],
        chunk_count: u32,
        lifecycle: LifecycleV2,
        assignment_height: u64,
    ) -> StorageFileInfoV2 {
        StorageFileInfoV2 {
            merkle_root: format!("0x{}", hex::encode(root)),
            owner: l1_address_base58(&[0x01; 20]),
            plaintext_size_bytes: 1024,
            stored_size_bytes: 1024,
            chunk_count,
            fee_pool: 1000,
            created_at: 100,
            activated_at_height: if lifecycle.is_active() { Some(150) } else { None },
            abandoned_at_height: None,
            assignment_height,
            visibility: VisibilityV2::PUBLIC,
            lifecycle,
            access_list: vec![],
        }
    }

    /// Convenience: spin up a validator with a snapshot, file row, and
    /// known-good chunk_index assigned to the chosen archive.
    struct ValidatorFixture {
        validator: PushValidator<MockRpc>,
        my_addr: [u8; 20],
        snapshot: Vec<[u8; 20]>,
        tree: TreeFixture,
        good_index: u32,
        bad_index: u32, // assigned to someone else but in-range
    }

    fn build_fixture(lifecycle: LifecycleV2) -> ValidatorFixture {
        let snapshot = five_archives();
        let my_addr = snapshot[2]; // pick the middle archive
        let tree = TreeFixture::new(16, 0x11);

        let good_index = first_chunk_assigned_to(
            &tree.root,
            &snapshot,
            tree.chunk_count,
            &my_addr,
            V2Params::DEFAULTS.assignment_replication_factor,
        );
        let bad_index = first_chunk_not_assigned_to(
            &tree.root,
            &snapshot,
            tree.chunk_count,
            &my_addr,
            V2Params::DEFAULTS.assignment_replication_factor,
        );

        let rpc = MockRpc::default();
        let assignment_height = 500;
        rpc.add_file(
            &format!("0x{}", hex::encode(tree.root)),
            file_info(&tree.root, tree.chunk_count, lifecycle, assignment_height),
        );
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
        );

        let validator = PushValidator::new(rpc, my_addr, V2Params::DEFAULTS);
        ValidatorFixture {
            validator,
            my_addr,
            snapshot,
            tree,
            good_index,
            bad_index,
        }
    }

    // ── Required tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_pending_validates() {
        let fx = build_fixture(LifecycleV2::PENDING);
        let i = fx.good_index;
        let validated = fx
            .validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .expect("happy path must validate");
        assert_eq!(
            validated.leaf_hash,
            *blake3::hash(fx.tree.data(i)).as_bytes()
        );
    }

    #[tokio::test]
    async fn happy_path_active_validates() {
        let fx = build_fixture(LifecycleV2::ACTIVE);
        let i = fx.good_index;
        fx.validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .expect("Active lifecycle must accept pushes");
    }

    #[tokio::test]
    async fn unknown_root_rejects() {
        // Build the validator, but never register the root in the mock.
        let snapshot = five_archives();
        let my_addr = snapshot[2];
        let tree = TreeFixture::new(8, 0x22);
        let rpc = MockRpc::default();
        // No file_info entry — RPC will return Err("unknown root").
        let validator = PushValidator::new(rpc, my_addr, V2Params::DEFAULTS);

        let err = validator
            .validate_push(tree.root, 0, tree.data(0), &tree.proof(0))
            .await
            .expect_err("unregistered root must reject");
        assert!(matches!(err, PushReject::UnknownRoot(_)),
                "got {err:?}, expected UnknownRoot");
    }

    #[tokio::test]
    async fn abandoned_lifecycle_rejects() {
        let fx = build_fixture(LifecycleV2::ABANDONED);
        let i = fx.good_index;
        let err = fx
            .validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .expect_err("Abandoned lifecycle must reject");
        assert!(matches!(err, PushReject::Abandoned), "got {err:?}");
    }

    #[tokio::test]
    async fn out_of_range_index_rejects_before_snapshot_lookup() {
        let fx = build_fixture(LifecycleV2::ACTIVE);
        // chunk_index = chunk_count is the smallest out-of-range value.
        let bad_index = fx.tree.chunk_count;
        let err = fx
            .validator
            .validate_push(fx.tree.root, bad_index, b"junk", &[])
            .await
            .expect_err("chunk_index >= chunk_count must reject");
        match err {
            PushReject::OutOfRange { chunk_index, chunk_count } => {
                assert_eq!(chunk_index, bad_index);
                assert_eq!(chunk_count, fx.tree.chunk_count);
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }

        // Confirm we did NOT fetch the snapshot — the bound is checked
        // before that RPC fires.
        assert_eq!(
            fx.validator.rpc.snapshot_call_count(),
            0,
            "OutOfRange must short-circuit before snapshot RPC"
        );
    }

    #[tokio::test]
    async fn not_in_snapshot_rejects() {
        // Same fixture, but instantiate the validator with an address
        // that's not in the canonical snapshot.
        let snapshot = five_archives();
        let stranger = [0xFE; 20]; // not in the snapshot
        let tree = TreeFixture::new(8, 0x33);
        let assignment_height = 700;

        let rpc = MockRpc::default();
        rpc.add_file(
            &format!("0x{}", hex::encode(tree.root)),
            file_info(
                &tree.root,
                tree.chunk_count,
                LifecycleV2::ACTIVE,
                assignment_height,
            ),
        );
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
        );
        let validator = PushValidator::new(rpc, stranger, V2Params::DEFAULTS);

        let err = validator
            .validate_push(tree.root, 0, tree.data(0), &tree.proof(0))
            .await
            .expect_err("address not in snapshot must reject");
        match err {
            PushReject::NotInSnapshot { height, .. } => assert_eq!(height, assignment_height),
            other => panic!("expected NotInSnapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_assigned_to_chunk_rejects() {
        let fx = build_fixture(LifecycleV2::ACTIVE);
        // We're in the snapshot, but pick a chunk assigned to other
        // archives only.
        let i = fx.bad_index;
        let err = fx
            .validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .expect_err("chunk not assigned to this archive must reject");
        match err {
            PushReject::NotAssigned { chunk_index } => assert_eq!(chunk_index, i),
            other => panic!("expected NotAssigned, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_merkle_proof_rejects() {
        let fx = build_fixture(LifecycleV2::ACTIVE);
        let i = fx.good_index;
        // Tamper one byte in the proof (or substitute junk siblings).
        let mut bad_proof = fx.tree.proof(i);
        if bad_proof.is_empty() {
            // 1-leaf tree edge case — substitute an obviously wrong
            // proof of length 1 (which violates expected_proof_depth(1) = 0).
            bad_proof = vec![[0xFF; 32]];
        } else {
            bad_proof[0][0] ^= 0x01;
        }
        let err = fx
            .validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &bad_proof)
            .await
            .expect_err("tampered proof must reject");
        assert!(matches!(err, PushReject::BadProof), "got {err:?}");
    }

    #[tokio::test]
    async fn tampered_data_rejects_via_proof() {
        // Tamper data → leaf_hash changes → strict proof verifier fails.
        // This is BadProof, not BadShape, since the proof is of the
        // right length and the shape is fine.
        let fx = build_fixture(LifecycleV2::ACTIVE);
        let i = fx.good_index;
        let mut tampered: Vec<u8> = fx.tree.data(i).to_vec();
        tampered[0] ^= 0xFF;
        let err = fx
            .validator
            .validate_push(fx.tree.root, i, &tampered, &fx.tree.proof(i))
            .await
            .expect_err("tampered data must reject as BadProof");
        assert!(matches!(err, PushReject::BadProof), "got {err:?}");
    }

    #[tokio::test]
    async fn wrong_merkle_root_in_request_is_unknown_root() {
        // Push carries a root the chain doesn't know. We register a
        // *different* root in the mock so the requested one is unknown.
        let fx = build_fixture(LifecycleV2::ACTIVE);
        // Build a different tree with a different root.
        let other_tree = TreeFixture::new(4, 0x99);
        assert_ne!(other_tree.root, fx.tree.root);
        let i = fx.good_index;

        let err = fx
            .validator
            .validate_push(
                other_tree.root,
                0,
                other_tree.data(0),
                &other_tree.proof(0),
            )
            .await
            .expect_err("wire root not registered → reject");
        assert!(matches!(err, PushReject::UnknownRoot(_)), "got {err:?}");
        // Sanity: the original root still validates from the same
        // validator — we didn't poison anything.
        fx.validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .expect("valid root should still validate");
    }

    /// Caches: second push with the same root + same assignment_height
    /// must NOT re-RPC. Regression guard against accidentally bypassing
    /// the cache (e.g. a refactor that rebuilds the validator per call).
    #[tokio::test]
    async fn cache_avoids_redundant_rpcs() {
        let fx = build_fixture(LifecycleV2::ACTIVE);
        let i = fx.good_index;
        // Find a second chunk_index also assigned to my_addr so we can
        // reuse the same root + snapshot.
        let mut second_index = None;
        for cand in 0..fx.tree.chunk_count {
            if cand == fx.good_index {
                continue;
            }
            let assigned = assigned_archives(
                &fx.tree.root,
                &fx.snapshot,
                cand,
                V2Params::DEFAULTS.assignment_replication_factor,
            );
            if assigned.iter().any(|a| a == &fx.my_addr) {
                second_index = Some(cand);
                break;
            }
        }
        let second_index = second_index.expect("need a second chunk assigned to my_addr");

        fx.validator
            .validate_push(fx.tree.root, i, fx.tree.data(i), &fx.tree.proof(i))
            .await
            .unwrap();
        let after_first_file = fx.validator.rpc.file_info_call_count();
        let after_first_snap = fx.validator.rpc.snapshot_call_count();
        assert_eq!(after_first_file, 1);
        assert_eq!(after_first_snap, 1);

        fx.validator
            .validate_push(
                fx.tree.root,
                second_index,
                fx.tree.data(second_index),
                &fx.tree.proof(second_index),
            )
            .await
            .unwrap();
        assert_eq!(
            fx.validator.rpc.file_info_call_count(),
            after_first_file,
            "second push with same root must hit file_info cache"
        );
        assert_eq!(
            fx.validator.rpc.snapshot_call_count(),
            after_first_snap,
            "second push at same assignment_height must hit snapshot cache"
        );
    }

    /// Bad chain shape: a snapshot RPC returns a node with a malformed
    /// (non-base58) address. Validator must surface BadChainShape and
    /// not silently drop the bad entry.
    #[tokio::test]
    async fn malformed_snapshot_address_rejects_as_bad_shape() {
        let snapshot = five_archives();
        let my_addr = snapshot[2];
        let tree = TreeFixture::new(8, 0x44);
        let assignment_height = 800;

        let mut nodes: Vec<NodeRecordInfo> = snapshot.iter().map(node_record).collect();
        nodes.push(NodeRecordInfo {
            address: "not-valid-base58-???".into(),
            role: "ArchiveNode".into(),
            staked_balance: 0,
            status: "Active".into(),
            registered_at: 1,
        });

        let rpc = MockRpc::default();
        rpc.add_file(
            &format!("0x{}", hex::encode(tree.root)),
            file_info(
                &tree.root,
                tree.chunk_count,
                LifecycleV2::ACTIVE,
                assignment_height,
            ),
        );
        rpc.add_snapshot(assignment_height, nodes);
        let validator = PushValidator::new(rpc, my_addr, V2Params::DEFAULTS);

        let err = validator
            .validate_push(tree.root, 0, tree.data(0), &tree.proof(0))
            .await
            .expect_err("malformed snapshot must reject as BadChainShape");
        assert!(matches!(err, PushReject::BadChainShape(_)), "got {err:?}");
    }

    /// Bad chain shape: file_info response carries a `merkle_root` that
    /// doesn't match what we asked about. Must surface BadChainShape.
    #[tokio::test]
    async fn mismatched_response_root_rejects_as_bad_shape() {
        let snapshot = five_archives();
        let my_addr = snapshot[2];
        let tree = TreeFixture::new(8, 0x55);
        let assignment_height = 900;

        let mut info = file_info(
            &tree.root,
            tree.chunk_count,
            LifecycleV2::ACTIVE,
            assignment_height,
        );
        // Corrupt the response's own root field — chain bug or RPC misroute.
        info.merkle_root = format!("0x{}", "ab".repeat(32));

        let rpc = MockRpc::default();
        rpc.add_file(&format!("0x{}", hex::encode(tree.root)), info);
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
        );
        let validator = PushValidator::new(rpc, my_addr, V2Params::DEFAULTS);

        let err = validator
            .validate_push(tree.root, 0, tree.data(0), &tree.proof(0))
            .await
            .expect_err("mismatched response root must reject");
        assert!(matches!(err, PushReject::BadChainShape(_)), "got {err:?}");
    }
}
