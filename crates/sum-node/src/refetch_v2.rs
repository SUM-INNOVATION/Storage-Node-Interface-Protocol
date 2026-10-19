//! V2 re-fetch by a newly assigned archive (G3 P3), Public files only.
//!
//! After a `ReassignChunksV2`, the latest epoch's snapshot can assign this
//! archive chunks it was not assigned under the previous epoch. Nobody pushes
//! them: the owner's `ingest`/`resume` still route to the original snapshot.
//! This module plans and performs the fetch from archives that should hold
//! them.
//!
//! 1. **Plan** ([`plan_refetch`]). For a file with at least two assignment
//!    epochs and this archive in the latest snapshot, the newly assigned
//!    chunks are those assigned to it under the latest epoch, not assigned to
//!    it under the previous epoch, and not already held. Chunks it was
//!    already assigned are not fetched: completing the original placement is
//!    the owner's push, not P3. Sources for each chunk, in order: its other
//!    assignees under the latest epoch, then its assignees under earlier
//!    epochs, newest first.
//! 2. **Manifest.** The locally indexed manifest, or one pulled from a source
//!    and validated exactly as a Public ManifestPush (the recomputed Merkle
//!    root equals the committed root, CIDs bind to their blake3 hashes), with
//!    the chain's chunk count.
//! 3. **Chunks.** Pulled bytes must hash to the manifest's leaf hash for
//!    their index. Corrupt or substituted bytes are refused and the next
//!    source is tried. Verified bytes then go through the push validator with
//!    a Merkle path computed from the validated manifest (chain registration,
//!    latest-epoch assignment to this archive, strict proof), and are stored
//!    and recorded as held exactly like a push.
//! 4. **Attest.** Nothing here signs. The epoch watcher attests held chunks
//!    under the latest epoch at its next tick.
//!
//! Bounded per run: files read, chunks fetched, sources tried per chunk,
//! epochs consulted. There is no deletion path. No latency is promised: when
//! a newly assigned archive becomes challengeable is protocol decision P5,
//! which is unratified.
//!
//! Private files are skipped. Their manifests and ciphertext chunks are
//! pull-gated by the file's access list, and whether a newly assigned archive
//! may pull them is an open protocol decision.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sum_store::assignment_v2::{assigned_archives, chunks_for_archive_v2};
use sum_store::merkle::MerkleTree;
use sum_types::rpc_types::PushableFileInfoV2;
use sum_types::storage::DataManifest;
use tracing::{debug, info, warn};

use crate::inbound_v2::{AttestTriggerRpc, FileShape};
use crate::rpc_client::L1RpcClient;

/// Epochs consulted per file: the latest, the previous, and older ones as
/// extra sources.
pub const MAX_EPOCHS_CONSULTED: usize = 4;

/// The chain listing a re-fetch scans for files.
#[async_trait]
pub trait PushableFilesRpc: Send + Sync {
    /// One page of `storage_getPushableFilesV2`.
    async fn pushable_files(&self, offset: u32, limit: u32) -> Result<Vec<PushableFileInfoV2>>;
}

#[async_trait]
impl PushableFilesRpc for L1RpcClient {
    async fn pushable_files(&self, offset: u32, limit: u32) -> Result<Vec<PushableFileInfoV2>> {
        L1RpcClient::storage_get_pushable_files_v2(self, Some(offset), Some(limit)).await
    }
}

/// Pulls from other archives, addressed by chain address.
#[async_trait]
pub trait RefetchTransport: Send + Sync {
    /// The raw manifest bytes `from` serves for `merkle_root`.
    async fn pull_manifest(&self, from: [u8; 20], merkle_root: [u8; 32]) -> Result<Vec<u8>>;
    /// The bytes `from` serves for `cid`, which the manifest says are `size`
    /// bytes long.
    async fn pull_chunk(&self, from: [u8; 20], cid: &str, size: u64) -> Result<Vec<u8>>;
}

#[async_trait]
impl<T: RefetchTransport + ?Sized> RefetchTransport for Arc<T> {
    async fn pull_manifest(&self, from: [u8; 20], merkle_root: [u8; 32]) -> Result<Vec<u8>> {
        (**self).pull_manifest(from, merkle_root).await
    }
    async fn pull_chunk(&self, from: [u8; 20], cid: &str, size: u64) -> Result<Vec<u8>> {
        (**self).pull_chunk(from, cid, size).await
    }
}

/// Where verified results go: the archive's own push-acceptance path.
#[async_trait]
pub trait RefetchSink: Send + Sync {
    /// Chunk indices of `merkle_root` this archive holds.
    fn held_for(&self, merkle_root: &[u8; 32]) -> BTreeSet<u32>;
    /// The locally indexed Public manifest, if any.
    async fn local_manifest(&self, merkle_root: &[u8; 32]) -> Option<DataManifest>;
    /// Validate and index a pulled Public manifest.
    async fn accept_manifest(
        &self,
        merkle_root: [u8; 32],
        chain_chunk_count: u32,
        manifest_bytes: &[u8],
    ) -> std::result::Result<DataManifest, String>;
    /// Validate, store and record a chunk as a V2 Push would be, refusing a
    /// Private file's chunk. `None` on success, else the refusal reason.
    async fn accept_chunk(
        &self,
        from: [u8; 20],
        merkle_root: [u8; 32],
        chunk_index: u32,
        data: Vec<u8>,
        merkle_path: Vec<[u8; 32]>,
    ) -> Option<String>;
}

#[async_trait]
impl<V, A, T> RefetchSink for crate::inbound_v2::V2Dispatcher<V, A, T>
where
    V: crate::push_validator::V2RpcClient + 'static,
    A: crate::assignment_attestor::AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    fn held_for(&self, merkle_root: &[u8; 32]) -> BTreeSet<u32> {
        crate::inbound_v2::V2Dispatcher::held_for(self, merkle_root)
    }
    async fn local_manifest(&self, merkle_root: &[u8; 32]) -> Option<DataManifest> {
        crate::inbound_v2::V2Dispatcher::local_manifest(self, merkle_root).await
    }
    async fn accept_manifest(
        &self,
        merkle_root: [u8; 32],
        chain_chunk_count: u32,
        manifest_bytes: &[u8],
    ) -> std::result::Result<DataManifest, String> {
        self.accept_refetched_manifest(merkle_root, chain_chunk_count, manifest_bytes)
            .await
    }
    async fn accept_chunk(
        &self,
        from: [u8; 20],
        merkle_root: [u8; 32],
        chunk_index: u32,
        data: Vec<u8>,
        merkle_path: Vec<[u8; 32]>,
    ) -> Option<String> {
        let source = format!("re-fetch from 0x{}", hex::encode(from));
        self.accept_refetched_chunk(&source, data, merkle_root, chunk_index, merkle_path)
            .await
    }
}

/// One chunk to fetch and where to try, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    pub chunk_index: u32,
    pub sources: Vec<[u8; 20]>,
}

/// The chunks newly assigned to `me` under the latest epoch that it does not
/// hold, each with its sources. `epochs` is `(height, snapshot)` in ascending
/// height order, latest last. Empty unless there are at least two epochs and
/// `me` is in the latest snapshot.
pub fn plan_refetch(
    merkle_root: &[u8; 32],
    chunk_count: u32,
    epochs: &[(u64, Vec<[u8; 20]>)],
    replication_factor: u32,
    me: &[u8; 20],
    held: &BTreeSet<u32>,
) -> Vec<ChunkPlan> {
    let [.., (_, previous), (_, latest)] = epochs else {
        return Vec::new();
    };
    if !latest.contains(me) {
        return Vec::new();
    }
    let now = chunks_for_archive_v2(merkle_root, chunk_count, latest, replication_factor, me);
    let before = chunks_for_archive_v2(merkle_root, chunk_count, previous, replication_factor, me);
    now.difference(&before)
        .filter(|i| !held.contains(i))
        .map(|&chunk_index| {
            let mut sources: Vec<[u8; 20]> = Vec::new();
            for (_, snapshot) in epochs.iter().rev() {
                for a in assigned_archives(merkle_root, snapshot, chunk_index, replication_factor) {
                    if a != *me && !sources.contains(&a) {
                        sources.push(a);
                    }
                }
            }
            ChunkPlan {
                chunk_index,
                sources,
            }
        })
        .collect()
}

/// Per-run bounds.
#[derive(Debug, Clone, Copy)]
pub struct RefetchLimits {
    /// Pushable-file rows read per run.
    pub files_per_run: usize,
    /// Chunks fetched (or attempted) per run.
    pub chunks_per_run: usize,
    /// Sources tried per chunk, and per manifest.
    pub sources_per_chunk: usize,
    /// Page size for `storage_getPushableFilesV2` (chain cap 1024).
    pub page: u32,
}

impl Default for RefetchLimits {
    fn default() -> Self {
        Self {
            files_per_run: 256,
            chunks_per_run: 1024,
            sources_per_chunk: 3,
            page: 256,
        }
    }
}

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefetchReport {
    pub files_checked: usize,
    pub private_skipped: usize,
    /// Files whose chain rows could not be read or were inconsistent.
    pub errors: usize,
    pub planned: usize,
    pub fetched: Vec<([u8; 32], u32)>,
    /// No source delivered a chunk the archive accepted.
    pub failed: Vec<([u8; 32], u32)>,
    /// Pulled bytes that did not hash to the manifest's leaf.
    pub corrupt_refused: usize,
    /// Verified bytes the push validator still refused (chain state).
    pub validator_refused: usize,
    pub manifest_refused: usize,
}

/// Fetches newly assigned Public chunks for one archive.
pub struct Refetcher<C, N, S> {
    chain: Arc<C>,
    net: N,
    sink: Arc<S>,
    me: [u8; 20],
    replication_factor: u32,
    limits: RefetchLimits,
    /// Private roots whose skip has been logged, so the reason is logged
    /// once per file rather than every run.
    private_noted: StdMutex<HashSet<[u8; 32]>>,
}

/// The one reason a Private file is not re-fetched.
pub const PRIVATE_SKIP_REASON: &str = "Private file not re-fetched: its manifest and ciphertext \
     are served only to the file's access list, and re-fetch of Private data awaits an \
     authorization policy";

impl<C, N, S> Refetcher<C, N, S>
where
    C: AttestTriggerRpc + PushableFilesRpc,
    N: RefetchTransport,
    S: RefetchSink,
{
    pub fn new(
        chain: Arc<C>,
        net: N,
        sink: Arc<S>,
        me: [u8; 20],
        replication_factor: u32,
        limits: RefetchLimits,
    ) -> Self {
        Self {
            chain,
            net,
            sink,
            me,
            replication_factor,
            limits,
            private_noted: StdMutex::new(HashSet::new()),
        }
    }

    /// Run [`Self::run_once`] every `interval` until `shutdown` changes. A
    /// zero interval disables re-fetch.
    pub async fn run(self, interval: Duration, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if interval.is_zero() {
            info!("V2 re-fetch disabled (interval 0)");
            return;
        }
        info!(
            interval_secs = interval.as_secs(),
            "V2 re-fetch started (Public files only)"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.run_once().await;
                }
                _ = shutdown.changed() => {
                    info!("V2 re-fetch stopping");
                    return;
                }
            }
        }
    }

    /// One bounded pass over the pushable-file listing.
    pub async fn run_once(&self) -> RefetchReport {
        let mut report = RefetchReport::default();
        let mut offset = 0u32;
        let page = self.limits.page.max(1);
        'pages: loop {
            let rows = match self.chain.pushable_files(offset, page).await {
                Ok(rows) => rows,
                Err(e) => {
                    warn!(%e, offset, "re-fetch: pushable-file listing failed");
                    report.errors += 1;
                    break;
                }
            };
            let returned = rows.len();
            for row in rows {
                if report.files_checked >= self.limits.files_per_run
                    || report.fetched.len() + report.failed.len() >= self.limits.chunks_per_run
                {
                    break 'pages;
                }
                report.files_checked += 1;
                match parse_root(&row.merkle_root) {
                    Some(root) => self.refetch_file(root, &mut report).await,
                    None => report.errors += 1,
                }
            }
            if returned < page as usize {
                break;
            }
            offset = offset.saturating_add(returned as u32);
        }
        if report.planned > 0 {
            info!(
                files = report.files_checked,
                planned = report.planned,
                fetched = report.fetched.len(),
                failed = report.failed.len(),
                corrupt = report.corrupt_refused,
                "re-fetch: run complete"
            );
        }
        report
    }

    async fn refetch_file(&self, root: [u8; 32], report: &mut RefetchReport) {
        let root_hex = hex::encode(root);
        let shape: FileShape = match self.chain.fetch_file_shape(&root).await {
            Ok(s) => s,
            Err(e) => {
                debug!(root = %root_hex, %e, "re-fetch: file shape unreadable");
                report.errors += 1;
                return;
            }
        };
        if shape.lifecycle.is_abandoned() {
            return;
        }
        if shape.visibility.is_private() {
            // Fail closed: nothing is requested, stored or attested for a
            // Private file through re-fetch.
            report.private_skipped += 1;
            if self
                .private_noted
                .lock()
                .expect("mutex poisoned")
                .insert(root)
            {
                info!(root = %root_hex, "{PRIVATE_SKIP_REASON}");
            }
            return;
        }
        let coverage = match self.chain.fetch_coverage(&root).await {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                debug!(root = %root_hex, %e, "re-fetch: coverage unreadable");
                report.errors += 1;
                return;
            }
        };
        let heights = &coverage.assignment_epochs;
        if heights.len() < 2 {
            return; // never reassigned: placement is the owner's push
        }
        let consistent = heights.windows(2).all(|w| w[0] < w[1])
            && heights.first() == Some(&shape.assignment_height)
            && heights.last() == Some(&coverage.latest_assignment_epoch);
        if !consistent {
            warn!(root = %root_hex, ?heights, "re-fetch: inconsistent assignment epochs; skipped");
            report.errors += 1;
            return;
        }
        let consulted = &heights[heights.len().saturating_sub(MAX_EPOCHS_CONSULTED)..];
        let mut epochs = Vec::with_capacity(consulted.len());
        for &h in consulted {
            match self.chain.fetch_snapshot(h).await {
                Ok(s) => epochs.push((h, s)),
                Err(e) => {
                    debug!(root = %root_hex, height = h, %e, "re-fetch: snapshot unreadable");
                    report.errors += 1;
                    return;
                }
            }
        }
        let held = self.sink.held_for(&root);
        let plan = plan_refetch(
            &root,
            shape.chunk_count,
            &epochs,
            self.replication_factor,
            &self.me,
            &held,
        );
        if plan.is_empty() {
            return;
        }
        report.planned += plan.len();

        let Some(manifest) = self
            .manifest_for(root, shape.chunk_count, &plan, report)
            .await
        else {
            for cp in &plan {
                report.failed.push((root, cp.chunk_index));
            }
            return;
        };
        let leaves: Vec<blake3::Hash> = manifest
            .chunks
            .iter()
            .map(|d| blake3::Hash::from(d.blake3_hash))
            .collect();
        let tree = MerkleTree::build(&leaves);

        for cp in plan {
            if report.fetched.len() + report.failed.len() >= self.limits.chunks_per_run {
                return;
            }
            let Some(desc) = manifest.chunks.get(cp.chunk_index as usize) else {
                report.failed.push((root, cp.chunk_index));
                continue;
            };
            let mut accepted = false;
            for &from in cp.sources.iter().take(self.limits.sources_per_chunk) {
                let data = match self.net.pull_chunk(from, &desc.cid, desc.size).await {
                    Ok(d) => d,
                    Err(e) => {
                        debug!(root = %root_hex, index = cp.chunk_index, %e, "re-fetch: pull failed");
                        continue;
                    }
                };
                if *blake3::hash(&data).as_bytes() != desc.blake3_hash {
                    warn!(
                        root = %root_hex,
                        index = cp.chunk_index,
                        from = %hex::encode(from),
                        "re-fetch: bytes do not match the manifest; refused"
                    );
                    report.corrupt_refused += 1;
                    continue;
                }
                match self
                    .sink
                    .accept_chunk(
                        from,
                        root,
                        cp.chunk_index,
                        data,
                        tree.proof_bytes(cp.chunk_index),
                    )
                    .await
                {
                    None => accepted = true,
                    Some(reason) => {
                        // The bytes match the committed manifest, so the
                        // refusal is about chain state; other sources would
                        // be refused the same way.
                        debug!(root = %root_hex, index = cp.chunk_index, %reason, "re-fetch: refused by the push validator");
                        report.validator_refused += 1;
                    }
                }
                break;
            }
            if accepted {
                report.fetched.push((root, cp.chunk_index));
            } else {
                report.failed.push((root, cp.chunk_index));
            }
        }
    }

    /// The local manifest, or the first pulled one that validates.
    async fn manifest_for(
        &self,
        root: [u8; 32],
        chunk_count: u32,
        plan: &[ChunkPlan],
        report: &mut RefetchReport,
    ) -> Option<DataManifest> {
        if let Some(m) = self.sink.local_manifest(&root).await {
            return Some(m);
        }
        let mut tried: Vec<[u8; 20]> = Vec::new();
        for from in plan.iter().flat_map(|cp| cp.sources.iter().copied()) {
            if tried.contains(&from) {
                continue;
            }
            if tried.len() >= self.limits.sources_per_chunk {
                break;
            }
            tried.push(from);
            let bytes = match self.net.pull_manifest(from, root).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            match self.sink.accept_manifest(root, chunk_count, &bytes).await {
                Ok(m) => return Some(m),
                Err(reason) => {
                    warn!(
                        root = %hex::encode(root),
                        from = %hex::encode(from),
                        %reason,
                        "re-fetch: manifest refused"
                    );
                    report.manifest_refused += 1;
                }
            }
        }
        None
    }
}

fn parse_root(s: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok()?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use sum_types::rpc_types::{AssignmentCoverageV2, LifecycleV2, VisibilityV2};
    use sum_types::storage::ChunkDescriptor;

    const R: u32 = 3;
    const CHUNKS: u32 = 48;

    fn archives() -> Vec<[u8; 20]> {
        (0..5u8)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xA0 + i;
                a
            })
            .collect()
    }

    /// Epoch 900: archive 4 left, `0xF0…` (the newly assigned archive) joined.
    fn joiner() -> [u8; 20] {
        [0xF0; 20]
    }
    fn epoch1() -> Vec<[u8; 20]> {
        let mut s = archives();
        s[4] = joiner();
        s.sort();
        s
    }

    fn chunk(i: u32, salt: u8) -> Vec<u8> {
        let mut v = vec![salt; 40];
        v.extend_from_slice(&i.to_le_bytes());
        v
    }

    /// A Public file of `CHUNKS` chunks: its manifest, chunk bytes and CBOR.
    fn file(salt: u8) -> (DataManifest, Vec<Vec<u8>>, Vec<u8>) {
        let chunks: Vec<Vec<u8>> = (0..CHUNKS).map(|i| chunk(i, salt)).collect();
        let leaves: Vec<blake3::Hash> = chunks.iter().map(|c| blake3::hash(c)).collect();
        let tree = MerkleTree::build(&leaves);
        let mut offset = 0;
        let descriptors = leaves
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let size = chunks[i].len() as u64;
                let d = ChunkDescriptor {
                    chunk_index: i as u32,
                    offset,
                    size,
                    blake3_hash: *h.as_bytes(),
                    cid: sum_store::content_id::cid_from_blake3_hash(h),
                    plaintext_blake3_hash: None,
                };
                offset += size;
                d
            })
            .collect();
        let manifest = DataManifest {
            file_name: format!("refetch-{salt:02x}.bin"),
            file_hash: *blake3::hash(&chunks.concat()).as_bytes(),
            total_size_bytes: offset,
            chunk_count: CHUNKS,
            merkle_root: *tree.root().as_bytes(),
            chunks: descriptors,
        };
        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&manifest, &mut cbor).unwrap();
        (manifest, chunks, cbor)
    }

    // ── Chain ────────────────────────────────────────────────────────────

    struct Chain {
        files: Vec<PushableFileInfoV2>,
        shapes: HashMap<[u8; 32], FileShape>,
        epochs: HashMap<[u8; 32], Vec<u64>>,
        snapshots: HashMap<u64, Vec<[u8; 20]>>,
    }

    impl Chain {
        fn new() -> Self {
            Self {
                files: vec![],
                shapes: HashMap::new(),
                epochs: HashMap::new(),
                snapshots: HashMap::from([(500, archives()), (900, epoch1())]),
            }
        }
        fn add(&mut self, root: [u8; 32], private: bool, epochs: &[u64]) {
            self.files.push(PushableFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(root)),
                chunk_count: CHUNKS,
                lifecycle: 1,
                created_at: 1,
            });
            self.shapes.insert(
                root,
                FileShape {
                    chunk_count: CHUNKS,
                    assignment_height: epochs[0],
                    visibility: if private {
                        VisibilityV2::PRIVATE
                    } else {
                        VisibilityV2::PUBLIC
                    },
                    lifecycle: LifecycleV2::ACTIVE,
                },
            );
            self.epochs.insert(root, epochs.to_vec());
        }
    }

    #[async_trait]
    impl AttestTriggerRpc for Chain {
        async fn fetch_file_shape(&self, root: &[u8; 32]) -> Result<FileShape> {
            self.shapes
                .get(root)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unregistered"))
        }
        async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>> {
            self.snapshots
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no snapshot"))
        }
        async fn fetch_nonce(&self, _a: &str) -> Result<u64> {
            Ok(0)
        }
        async fn fetch_coverage(&self, root: &[u8; 32]) -> Result<Option<AssignmentCoverageV2>> {
            let Some(epochs) = self.epochs.get(root) else {
                return Ok(None);
            };
            Ok(Some(AssignmentCoverageV2 {
                chunk_count: CHUNKS,
                covered_count: 0,
                can_activate_now: false,
                missing_total: 0,
                missing_offset: 0,
                missing_indices: vec![],
                per_archive: vec![],
                assignment_epochs: epochs.clone(),
                latest_assignment_epoch: *epochs.last().unwrap(),
                reassignment_needed: false,
                per_epoch: vec![],
            }))
        }
    }

    #[async_trait]
    impl PushableFilesRpc for Chain {
        async fn pushable_files(&self, offset: u32, limit: u32) -> Result<Vec<PushableFileInfoV2>> {
            Ok(self
                .files
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect())
        }
    }

    // ── Other archives ───────────────────────────────────────────────────

    #[derive(Default)]
    struct Net {
        /// What each archive serves for a CID; absent means "not found".
        chunks: HashMap<([u8; 20], String), Vec<u8>>,
        manifests: HashMap<[u8; 20], Vec<u8>>,
        pulls: StdMutex<Vec<([u8; 20], String)>>,
        manifest_pulls: StdMutex<Vec<[u8; 20]>>,
    }

    #[async_trait]
    impl RefetchTransport for Net {
        async fn pull_manifest(&self, from: [u8; 20], _root: [u8; 32]) -> Result<Vec<u8>> {
            self.manifest_pulls.lock().unwrap().push(from);
            self.manifests
                .get(&from)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no manifest"))
        }
        async fn pull_chunk(&self, from: [u8; 20], cid: &str, _size: u64) -> Result<Vec<u8>> {
            self.pulls.lock().unwrap().push((from, cid.to_string()));
            self.chunks
                .get(&(from, cid.to_string()))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
    }

    impl Net {
        /// Every archive serves the file's manifest and every chunk.
        fn honest(manifest: &DataManifest, chunks: &[Vec<u8>], cbor: &[u8]) -> Self {
            let mut n = Net::default();
            for a in archives().into_iter().chain([joiner()]) {
                n.manifests.insert(a, cbor.to_vec());
                for d in &manifest.chunks {
                    n.chunks
                        .insert((a, d.cid.clone()), chunks[d.chunk_index as usize].clone());
                }
            }
            n
        }
    }

    // ── This archive ─────────────────────────────────────────────────────

    /// Records accepted chunks and validates manifests with the real
    /// ManifestPush validator. `refuse` makes the push validator refuse.
    #[derive(Default)]
    struct Sink {
        manifest: StdMutex<Option<DataManifest>>,
        accepted: StdMutex<Vec<(u32, Vec<u8>, Vec<[u8; 32]>)>>,
        held: BTreeSet<u32>,
        refuse: bool,
    }

    #[async_trait]
    impl RefetchSink for Sink {
        fn held_for(&self, _root: &[u8; 32]) -> BTreeSet<u32> {
            self.held.clone()
        }
        async fn local_manifest(&self, _root: &[u8; 32]) -> Option<DataManifest> {
            self.manifest.lock().unwrap().clone()
        }
        async fn accept_manifest(
            &self,
            root: [u8; 32],
            chunk_count: u32,
            bytes: &[u8],
        ) -> std::result::Result<DataManifest, String> {
            let m = sum_store::serve::validate_manifest_push(&hex::encode(root), bytes)?;
            if m.chunk_count != chunk_count {
                return Err("chunk count".into());
            }
            *self.manifest.lock().unwrap() = Some(m.clone());
            Ok(m)
        }
        async fn accept_chunk(
            &self,
            _from: [u8; 20],
            _root: [u8; 32],
            index: u32,
            data: Vec<u8>,
            path: Vec<[u8; 32]>,
        ) -> Option<String> {
            if self.refuse {
                return Some("chunk not assigned to this archive".into());
            }
            self.accepted.lock().unwrap().push((index, data, path));
            None
        }
    }

    fn refetcher(
        chain: Chain,
        net: Net,
        sink: Sink,
        limits: RefetchLimits,
    ) -> (Refetcher<Chain, Net, Sink>, Arc<Sink>) {
        let sink = Arc::new(sink);
        (
            Refetcher::new(Arc::new(chain), net, sink.clone(), joiner(), R, limits),
            sink,
        )
    }

    // ── Plan ─────────────────────────────────────────────────────────────

    #[test]
    fn the_plan_is_the_latest_assignment_minus_the_previous_and_held() {
        let root = [0x21; 32];
        let epochs = vec![(500, archives()), (900, epoch1())];
        let survivor = archives()[0];
        let now = chunks_for_archive_v2(&root, CHUNKS, &epoch1(), R, &survivor);
        let before = chunks_for_archive_v2(&root, CHUNKS, &archives(), R, &survivor);
        let held: BTreeSet<u32> = before.iter().copied().take(2).collect();
        let plan = plan_refetch(&root, CHUNKS, &epochs, R, &survivor, &held);
        let planned: BTreeSet<u32> = plan.iter().map(|c| c.chunk_index).collect();
        let expected: BTreeSet<u32> = now.difference(&before).copied().collect();
        assert_eq!(planned, expected, "only newly assigned chunks");
        assert!(
            planned.is_disjoint(&before),
            "never chunks it was already assigned"
        );

        // The joiner: everything assigned to it now is new.
        let plan = plan_refetch(&root, CHUNKS, &epochs, R, &joiner(), &BTreeSet::new());
        let planned: BTreeSet<u32> = plan.iter().map(|c| c.chunk_index).collect();
        assert_eq!(
            planned,
            chunks_for_archive_v2(&root, CHUNKS, &epoch1(), R, &joiner())
        );
        assert!(!planned.is_empty());

        // Held chunks are not fetched again.
        let held: BTreeSet<u32> = planned.iter().copied().take(3).collect();
        let plan = plan_refetch(&root, CHUNKS, &epochs, R, &joiner(), &held);
        assert!(plan.iter().all(|c| !held.contains(&c.chunk_index)));
    }

    #[test]
    fn sources_are_latest_assignees_then_earlier_ones_never_this_archive() {
        let root = [0x22; 32];
        let epochs = vec![(500, archives()), (900, epoch1())];
        for cp in plan_refetch(&root, CHUNKS, &epochs, R, &joiner(), &BTreeSet::new()) {
            let latest: Vec<[u8; 20]> = assigned_archives(&root, &epoch1(), cp.chunk_index, R)
                .into_iter()
                .filter(|a| *a != joiner())
                .collect();
            assert_eq!(
                cp.sources[..latest.len()],
                latest[..],
                "latest assignees first"
            );
            for a in assigned_archives(&root, &archives(), cp.chunk_index, R) {
                assert!(cp.sources.contains(&a), "earlier assignees follow");
            }
            assert!(!cp.sources.contains(&joiner()));
            let unique: BTreeSet<_> = cp.sources.iter().collect();
            assert_eq!(unique.len(), cp.sources.len(), "no duplicates");
        }
    }

    #[test]
    fn nothing_is_planned_without_a_reassignment_or_outside_the_latest_snapshot() {
        let root = [0x23; 32];
        let none = BTreeSet::new();
        assert!(
            plan_refetch(
                &root,
                CHUNKS,
                &[(500, archives())],
                R,
                &archives()[0],
                &none
            )
            .is_empty()
        );
        let epochs = vec![(500, archives()), (900, epoch1())];
        assert!(
            plan_refetch(&root, CHUNKS, &epochs, R, &archives()[4], &none).is_empty(),
            "the departed archive fetches nothing"
        );
    }

    // ── Run ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_newly_assigned_archive_fetches_verifies_and_hands_over_its_chunks() {
        let (manifest, chunks, cbor) = file(0x31);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());

        let report = r.run_once().await;
        let expected = chunks_for_archive_v2(&root, CHUNKS, &epoch1(), R, &joiner());
        assert_eq!(report.planned, expected.len());
        assert_eq!(
            report
                .fetched
                .iter()
                .map(|(_, i)| *i)
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert!(report.failed.is_empty());
        {
            let got = sink
                .manifest
                .lock()
                .unwrap()
                .clone()
                .expect("manifest indexed");
            assert_eq!(got.merkle_root, manifest.merkle_root);
            assert_eq!(got.chunks, manifest.chunks);
        }

        let tree = MerkleTree::build(&chunks.iter().map(|c| blake3::hash(c)).collect::<Vec<_>>());
        for (i, data, path) in sink.accepted.lock().unwrap().iter() {
            assert_eq!(data, &chunks[*i as usize], "the chunk's own bytes");
            assert_eq!(
                path,
                &tree.proof_bytes(*i),
                "a proof against the committed root"
            );
        }
    }

    #[tokio::test]
    async fn corrupt_and_substituted_bytes_are_refused_and_the_next_source_is_tried() {
        let (manifest, chunks, cbor) = file(0x32);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let mut net = Net::honest(&manifest, &chunks, &cbor);
        let plan = plan_refetch(
            &root,
            CHUNKS,
            &[(500, archives()), (900, epoch1())],
            R,
            &joiner(),
            &BTreeSet::new(),
        );
        // First source of the first chunk serves flipped bytes; first source
        // of the second serves another chunk's (valid) bytes.
        let (a, b) = (&plan[0], &plan[1]);
        let cid_a = manifest.chunks[a.chunk_index as usize].cid.clone();
        let mut bad = chunks[a.chunk_index as usize].clone();
        bad[0] ^= 0xFF;
        net.chunks.insert((a.sources[0], cid_a), bad);
        let cid_b = manifest.chunks[b.chunk_index as usize].cid.clone();
        let other = chunks[((b.chunk_index + 1) % CHUNKS) as usize].clone();
        net.chunks.insert((b.sources[0], cid_b), other);

        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let report = r.run_once().await;
        assert_eq!(report.corrupt_refused, 2);
        assert!(report.failed.is_empty());
        for (i, data, _) in sink.accepted.lock().unwrap().iter() {
            assert_eq!(
                data, &chunks[*i as usize],
                "only verified bytes are handed over"
            );
        }
    }

    #[tokio::test]
    async fn when_every_source_is_bad_nothing_is_accepted_and_tries_are_bounded() {
        let (manifest, chunks, cbor) = file(0x33);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let mut net = Net::honest(&manifest, &chunks, &cbor);
        for (k, v) in net.chunks.iter_mut() {
            if k.0 != joiner() {
                v[0] ^= 0x01;
            }
        }
        let limits = RefetchLimits {
            sources_per_chunk: 2,
            ..RefetchLimits::default()
        };
        let (r, sink) = refetcher(chain, net, Sink::default(), limits);
        let report = r.run_once().await;
        assert!(report.fetched.is_empty());
        assert_eq!(report.failed.len(), report.planned);
        assert_eq!(
            report.corrupt_refused,
            2 * report.planned,
            "two sources per chunk"
        );
        assert!(sink.accepted.lock().unwrap().is_empty());
        let pulls = r.net.pulls.lock().unwrap().len();
        assert_eq!(pulls, 2 * report.planned);
    }

    #[tokio::test]
    async fn a_bad_manifest_is_refused_and_another_source_is_used() {
        let (manifest, chunks, cbor) = file(0x34);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let mut net = Net::honest(&manifest, &chunks, &cbor);
        // A manifest for another file, and a tampered one, from the first two
        // sources in the plan.
        let plan = plan_refetch(
            &root,
            CHUNKS,
            &[(500, archives()), (900, epoch1())],
            R,
            &joiner(),
            &BTreeSet::new(),
        );
        let first: Vec<[u8; 20]> = {
            let mut v = Vec::new();
            for a in plan.iter().flat_map(|c| c.sources.iter().copied()) {
                if !v.contains(&a) {
                    v.push(a);
                }
            }
            v
        };
        let (_, _, other_cbor) = file(0x35);
        net.manifests.insert(first[0], other_cbor);
        let mut tampered = manifest.clone();
        tampered.chunks[0].blake3_hash[0] ^= 1;
        let mut t = Vec::new();
        ciborium::ser::into_writer(&tampered, &mut t).unwrap();
        net.manifests.insert(first[1], t);

        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let report = r.run_once().await;
        assert_eq!(report.manifest_refused, 2);
        {
            let got = sink
                .manifest
                .lock()
                .unwrap()
                .clone()
                .expect("manifest indexed");
            assert_eq!(got.merkle_root, manifest.merkle_root);
            assert_eq!(got.chunks, manifest.chunks);
        }
        assert!(report.failed.is_empty() && !report.fetched.is_empty());
    }

    #[tokio::test]
    async fn no_valid_manifest_means_no_chunk_is_fetched() {
        let (manifest, chunks, cbor) = file(0x36);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let mut net = Net::honest(&manifest, &chunks, &cbor);
        net.manifests.clear();
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let report = r.run_once().await;
        assert!(report.fetched.is_empty());
        assert_eq!(report.failed.len(), report.planned);
        assert!(r.net.pulls.lock().unwrap().is_empty(), "no chunk pulled");
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_validator_refusal_is_not_retried_against_other_sources() {
        let (manifest, chunks, cbor) = file(0x37);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let sink = Sink {
            refuse: true,
            ..Sink::default()
        };
        let (r, _sink) = refetcher(chain, net, sink, RefetchLimits::default());
        let report = r.run_once().await;
        assert_eq!(report.validator_refused, report.planned);
        assert_eq!(report.failed.len(), report.planned);
        assert_eq!(
            r.net.pulls.lock().unwrap().len(),
            report.planned,
            "one pull each"
        );
    }

    #[tokio::test]
    async fn private_and_never_reassigned_files_are_skipped() {
        let (manifest, chunks, cbor) = file(0x38);
        let (other, _, _) = file(0x39);
        let mut chain = Chain::new();
        chain.add(manifest.merkle_root, true, &[500, 900]);
        chain.add(other.merkle_root, false, &[500]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let report = r.run_once().await;
        assert_eq!(report.files_checked, 2);
        assert_eq!(report.private_skipped, 1);
        assert_eq!(report.planned, 0);
        assert!(r.net.pulls.lock().unwrap().is_empty());
        assert!(sink.accepted.lock().unwrap().is_empty());
    }

    /// Fail closed: a Private file the latest epoch newly assigns to this
    /// archive is never requested (no manifest pull, no chunk pull), stored
    /// or handed over, run after run, and the skip is noted once.
    #[tokio::test]
    async fn a_private_file_is_never_fetched_even_when_newly_assigned() {
        let (manifest, chunks, cbor) = file(0x3C);
        let root = manifest.merkle_root;
        let epochs = [(500, archives()), (900, epoch1())];
        assert!(
            !plan_refetch(&root, CHUNKS, &epochs, R, &joiner(), &BTreeSet::new()).is_empty(),
            "fixture: the latest epoch newly assigns this archive chunks"
        );
        let mut chain = Chain::new();
        chain.add(root, true, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        for _ in 0..3 {
            let report = r.run_once().await;
            assert_eq!(report.private_skipped, 1);
            assert_eq!(report.planned, 0);
        }
        assert!(r.net.pulls.lock().unwrap().is_empty(), "no chunk requested");
        assert!(
            r.net.manifest_pulls.lock().unwrap().is_empty(),
            "no manifest requested"
        );
        assert!(
            sink.accepted.lock().unwrap().is_empty(),
            "nothing handed over"
        );
        assert!(sink.manifest.lock().unwrap().is_none(), "nothing indexed");
        assert_eq!(r.private_noted.lock().unwrap().len(), 1, "skip noted once");
    }

    #[tokio::test]
    async fn run_is_off_at_zero_and_stops_on_shutdown() {
        let (manifest, chunks, cbor) = file(0x3D);
        let mut chain = Chain::new();
        chain.add(manifest.merkle_root, false, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let (tx, rx) = tokio::sync::watch::channel(false);
        // Interval 0: returns at once and fetches nothing.
        tokio::time::timeout(Duration::from_secs(1), r.run(Duration::ZERO, rx.clone()))
            .await
            .expect("disabled run returns");
        assert!(sink.accepted.lock().unwrap().is_empty());

        let (manifest, chunks, cbor) = file(0x3E);
        let mut chain = Chain::new();
        chain.add(manifest.merkle_root, false, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let task = tokio::spawn(r.run(Duration::from_secs(3600), rx));
        // The first tick fires at once; then shutdown stops the loop.
        for _ in 0..200 {
            if !sink.accepted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !sink.accepted.lock().unwrap().is_empty(),
            "first run happened"
        );
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stops on shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn inconsistent_epochs_are_skipped() {
        let (manifest, chunks, cbor) = file(0x3A);
        let mut chain = Chain::new();
        chain.add(manifest.merkle_root, false, &[900, 500]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let (r, _sink) = refetcher(chain, net, Sink::default(), RefetchLimits::default());
        let report = r.run_once().await;
        assert_eq!(report.errors, 1);
        assert_eq!(report.planned, 0);
    }

    #[tokio::test]
    async fn the_chunk_budget_bounds_a_run() {
        let (manifest, chunks, cbor) = file(0x3B);
        let root = manifest.merkle_root;
        let mut chain = Chain::new();
        chain.add(root, false, &[500, 900]);
        let net = Net::honest(&manifest, &chunks, &cbor);
        let limits = RefetchLimits {
            chunks_per_run: 2,
            ..RefetchLimits::default()
        };
        let (r, sink) = refetcher(chain, net, Sink::default(), limits);
        let report = r.run_once().await;
        assert!(report.planned > 2);
        assert_eq!(report.fetched.len(), 2);
        assert_eq!(sink.accepted.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn listing_pages_are_followed_until_a_short_page() {
        let mut chain = Chain::new();
        let mut net = Net::default();
        for salt in 0x40..0x45u8 {
            let (m, c, cbor) = file(salt);
            chain.add(m.merkle_root, false, &[500, 900]);
            let honest = Net::honest(&m, &c, &cbor);
            net.manifests = honest.manifests;
            net.chunks.extend(honest.chunks);
        }
        let limits = RefetchLimits {
            page: 2,
            ..RefetchLimits::default()
        };
        let (r, _sink) = refetcher(chain, net, Sink::default(), limits);
        assert_eq!(r.run_once().await.files_checked, 5);
    }
}
