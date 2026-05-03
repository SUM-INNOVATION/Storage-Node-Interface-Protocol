//! V2 ingest pipeline (chain plan v3.2 §3.1–§3.6 owner-side).
//!
//! State machine S0–S6 as a pure pipeline: takes a file path, returns
//! a typed [`IngestOutcome`]. CLI wiring lives in W10b; abandon /
//! resume CLI commands live in W11; integration over a real swarm
//! lives in W12.
//!
//! ## Stage map
//!
//! ```text
//! S0  Chunk + build DataManifest + cache Merkle proofs
//! S1  RegisterFilePendingV2 → submit → wait Finalized
//! S2  storage_getFileInfoV2 → snapshot at assignment_height
//!     compute V2 assignment per chunk
//!     push chunks (parallel, 2 retries per (chunk, peer))
//!     require R distinct PushAcks per chunk
//! S3  ManifestPush to each *distinct* assigned archive
//!     require 1 ManifestPushAck per archive (deduped)
//! S4  poll storage_getAssignmentCoverageV2 every poll_interval
//!     exit on can_activate_now == true OR activation_wait_secs
//! S5  ActivateFileV2 → submit → wait Finalized
//! S6  Activated
//! ```
//!
//! ## Outcome semantics
//!
//! Failures map to either [`IngestOutcome::Failed`] (only when
//! `RegisterFilePendingV2` itself never finalizes — no chain state was
//! created, caller can retry from scratch) or
//! [`IngestOutcome::PendingNeedsAction`] (file is `Pending` on chain,
//! caller must `resume` or `abandon` via W11).
//!
//! W10a never abandons. The pipeline never escalates to S7.
//!
//! ## activation_grace_blocks is reporting-only here
//!
//! `activation_grace_blocks` (chain plan default = 50) governs (a)
//! when post-activation PoR challenges become valid and (b) when
//! `AbandonFileV2` is admissible. It is **not** a pre-activation
//! coverage timeout. S4 uses a wall-clock `activation_wait_secs`
//! instead. The grace value is plumbed through [`IngestParams`] only
//! so operator-facing reports (and W11 abandon decisions) can
//! reference it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use sum_net::{PeerId, ShardResponseV2, SumNet, SumNetEvent};
use sum_store::assignment_v2::assigned_archives;
use sum_store::chunker::BinaryChunker;
use sum_store::merkle::MerkleTree;
use sum_types::rpc_types::{
    AssignmentCoverageV2, BlockHeightInfo, ChainParamsInfo, StorageFileInfoV2,
};
use sum_types::storage::DataManifest;
use tracing::{debug, info, warn};

use crate::assignment_attestor::AttestorRpc;
use crate::push_validator::V2RpcClient;
use crate::tx_builder::{
    build_activate_file_v2_tx, build_register_file_pending_v2_tx,
};
use crate::tx_wait::{wait_for_finalized, TxWaitError};

// ── Public types ─────────────────────────────────────────────────────────────

/// Stages of the V2 ingest pipeline. Used in [`IngestOutcome`] for
/// failure attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestStage {
    /// S0–S1: chunking through `RegisterFilePendingV2` finality.
    Register,
    /// S2: chunk push + per-chunk replication.
    Push,
    /// S3: per-archive `ManifestPush`.
    ManifestPush,
    /// S4: coverage polling.
    Coverage,
    /// S5: `ActivateFileV2` finality.
    Activate,
}

/// Operator guidance for what to do with a `Pending` file the
/// pipeline left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestedAction {
    /// W11 `resume` — re-query coverage and retry the residual
    /// chunks/manifest/activation.
    Resume,
    /// W11 `abandon` — submit `AbandonFileV2`. Used for fundamentally
    /// stuck files (e.g. assignment can never be reached because the
    /// snapshot has too few archives).
    Abandon,
}

/// Terminal outcome of [`IngestPipeline::run`] / [`IngestPipeline::resume`].
#[derive(Debug)]
pub enum IngestOutcome {
    /// Full happy path — we just submitted RegisterPending + Activate
    /// ourselves and recorded both tx hashes. **Only emitted by `run()`**;
    /// resume cannot reproduce this shape because chain state doesn't
    /// expose historical tx hashes.
    Activated {
        merkle_root: [u8; 32],
        manifest: DataManifest,
        register_tx_hash: String,
        register_height: u64,
        activate_tx_hash: String,
        activate_height: u64,
    },
    /// **Resume-only.** Chain state shows the file is already `Active`,
    /// but tx hashes are NOT recoverable — chain plan §4
    /// `StorageFileInfoV2` exposes `created_at` and
    /// `activated_at_height`, not historical tx hashes. Operators
    /// needing the tx hashes must consult their submission logs or a
    /// chain indexer. Carries the chain-recorded heights as
    /// `register_height`/`activate_height`.
    ActivatedOnChain {
        merkle_root: [u8; 32],
        manifest: DataManifest,
        register_height: u64,
        activate_height: u64,
    },
    /// **Resume-only.** Resume found the file in `Pending` state and
    /// successfully ran activate. We have a live `activate_tx_hash`
    /// from our own submit; `register_tx_hash` is NOT recoverable from
    /// chain state, so this variant intentionally omits it (no
    /// sentinel placeholder). `register_height` is the chain-recorded
    /// `created_at`.
    ResumedActivated {
        merkle_root: [u8; 32],
        manifest: DataManifest,
        register_height: u64,
        activate_tx_hash: String,
        activate_height: u64,
    },
    /// **Resume-only.** Chain state shows `lifecycle == Abandoned`.
    /// Terminal — not retryable.
    ///
    /// `abandoned_at_height` requires chain v3.3+ (post W11 follow-up
    /// binary). Pre-v3.3 chains don't surface the field, in which case
    /// callers see `None` here. Treat `None` defensively as
    /// "abandoned-height unavailable" — NOT a retryable error.
    AbandonedOnChain {
        merkle_root: [u8; 32],
        manifest: DataManifest,
        abandoned_at_height: Option<u64>,
    },
    /// **Resume-only.** Caller passed an explicit `merkle_root` plus a
    /// `file_path` whose computed root differs. Never recoverable by
    /// retrying — operator likely passed the wrong file for the recorded
    /// pending root.
    RootMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
        manifest: DataManifest,
    },
    /// `RegisterFilePendingV2` itself never finalized. No chain state
    /// was created — caller can rebuild a fresh tx (new nonce) and
    /// retry. Manifest is included so the caller doesn't have to
    /// re-chunk.
    Failed {
        stage: IngestStage,
        manifest: Option<DataManifest>,
        source: anyhow::Error,
    },
    /// File is `Pending` on chain. `resume` or `abandon` must run
    /// to clear it.
    PendingNeedsAction {
        merkle_root: [u8; 32],
        manifest: DataManifest,
        failed_stage: IngestStage,
        last_coverage: Option<AssignmentCoverageV2>,
        under_replicated_chunks: Option<Vec<u32>>,
        suggested: SuggestedAction,
        source: Option<anyhow::Error>,
    },
}

/// Terminal outcome of [`IngestPipeline::abandon`].
#[derive(Debug)]
pub enum AbandonOutcome {
    /// `AbandonFileV2` finalized at `finalized_at_height`.
    Abandoned {
        tx_hash: String,
        finalized_at_height: u64,
    },
    /// Pre-check rejected the abandon attempt — saved a wasted tx fee.
    /// Returned for either:
    /// * `lifecycle != Pending` (already Active or Abandoned) — chain
    ///   would reject with receipt code 31.
    /// * `current_height < earliest_admissible_height`, where
    ///   `earliest_admissible_height = info.created_at +
    ///    activation_grace_blocks + 1` per chain plan §3.5 strict-`>`
    ///   rule. Operator can re-call once the chain advances.
    NotAdmissible {
        reason: String,
        current_height: u64,
        earliest_admissible_height: u64,
    },
    /// Submission or finality failure on a pre-check-passing call.
    /// `source` carries the `send_raw_transaction` error or
    /// [`crate::tx_wait::TxWaitError`].
    Failed { source: anyhow::Error },
}

/// Pipeline-controllable parameters. None are hardcoded inside the
/// state machine — caller decides defaults vs. live chain values.
#[derive(Debug, Clone)]
pub struct IngestParams {
    /// `R` — number of archives each chunk is assigned to.
    pub assignment_replication_factor: u32,
    /// Cap on `chunk_indices.len()` per `AcceptAssignmentV2` tx.
    /// Forwarded into the V2 receive-side attestor; the ingest
    /// pipeline itself does not submit attestations.
    pub max_chunk_indices_per_tx: u32,
    /// Cap on `chunk_count` per file (chain plan §3.4 default
    /// 1,048,576). Pipeline rejects ingest above this so we don't
    /// build txs the chain will reject at validity.
    pub max_chunk_count_per_file: u32,
    /// Chain ID for tx signing.
    pub chain_id: u64,
    /// Per-tx fee in Koppa base units. Must be ≥ chain `min_fee`.
    pub fee_per_tx: u128,
    /// S4 coverage poll cadence. Default 2s.
    pub poll_interval: Duration,
    /// **S4 wall-clock timeout.** Pipeline returns
    /// `PendingNeedsAction { failed_stage: Coverage }` if
    /// `can_activate_now` doesn't latch within this window. NOT
    /// `activation_grace_blocks * block_time_ms` — see module doc.
    pub activation_wait_secs: Duration,
    /// Per-batch finality timeout for `wait_for_finalized` (S1, S5).
    pub finality_timeout: Duration,
    /// Push retry budget per `(chunk, peer)`. Default 2.
    pub push_retries: u32,
    /// S2 push wall-clock timeout per chunk-batch wave. The pipeline
    /// loops on `next_event` for this long before declaring
    /// under-replication; on timeout, missing chunks become the
    /// `under_replicated_chunks` set.
    pub push_wait_secs: Duration,
    /// S3 manifest-push wall-clock timeout. Same shape as
    /// `push_wait_secs` but per-archive.
    pub manifest_push_wait_secs: Duration,
    /// Reporting-only. Carries the chain's `activation_grace_blocks`
    /// so operators (and W11 abandon decisions) can reason about when
    /// `AbandonFileV2` is admissible. **Not used to control S4
    /// termination.**
    pub activation_grace_blocks: u64,
}

impl IngestParams {
    /// Populate from a live `chain_getChainParams` response. Fills the
    /// chain-locked fields; per-call wall-clocks (`poll_interval`,
    /// `activation_wait_secs`, etc.) keep the supplied defaults so the
    /// caller can tune cadence without editing chain values.
    pub fn from_chain_params(p: &ChainParamsInfo, defaults: IngestParamsDefaults) -> Self {
        Self {
            assignment_replication_factor: p.assignment_replication_factor,
            max_chunk_indices_per_tx: p.max_chunk_indices_per_tx,
            max_chunk_count_per_file: p.max_chunk_count_per_file,
            chain_id: p.chain_id,
            fee_per_tx: p.min_fee,
            poll_interval: defaults.poll_interval,
            activation_wait_secs: defaults.activation_wait_secs,
            finality_timeout: defaults.finality_timeout,
            push_retries: defaults.push_retries,
            push_wait_secs: defaults.push_wait_secs,
            manifest_push_wait_secs: defaults.manifest_push_wait_secs,
            activation_grace_blocks: p.activation_grace_blocks,
        }
    }
}

/// Wall-clock + retry knobs supplied separately from chain params —
/// these stay caller-tunable even when the rest comes from the chain.
#[derive(Debug, Clone, Copy)]
pub struct IngestParamsDefaults {
    pub poll_interval: Duration,
    pub activation_wait_secs: Duration,
    pub finality_timeout: Duration,
    pub push_retries: u32,
    pub push_wait_secs: Duration,
    pub manifest_push_wait_secs: Duration,
}

impl Default for IngestParamsDefaults {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            activation_wait_secs: Duration::from_secs(120),
            finality_timeout: Duration::from_secs(60),
            push_retries: 2,
            push_wait_secs: Duration::from_secs(60),
            manifest_push_wait_secs: Duration::from_secs(30),
        }
    }
}

// ── Trait abstractions ──────────────────────────────────────────────────────

/// Superset RPC interface the ingest pipeline consumes. Composed of
/// the existing W4/W6/W9 traits plus two coverage/height calls
/// W10 needs.
#[async_trait]
pub trait V2IngestRpc: AttestorRpc + V2RpcClient + Send + Sync {
    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
        missing_offset: Option<u32>,
        missing_limit: Option<u32>,
    ) -> Result<AssignmentCoverageV2>;
    async fn chain_get_block_height(&self) -> Result<BlockHeightInfo>;
    async fn get_nonce(&self, addr_base58: &str) -> Result<u64>;
}

/// Outbound V2 network operations the pipeline needs. Production impl
/// on `SumNet`; tests provide a scripted mock.
#[async_trait]
pub trait V2IngestNet: Send + Sync {
    async fn push_chunk_v2(
        &self,
        peer_id: PeerId,
        data: Vec<u8>,
        merkle_root: [u8; 32],
        chunk_index: u32,
        merkle_path: Vec<[u8; 32]>,
    ) -> Result<()>;

    async fn push_manifest_v2(
        &self,
        peer_id: PeerId,
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
    ) -> Result<()>;

    /// Pull the next inbound event from the swarm. Returns `None` when
    /// the swarm has shut down.
    async fn next_event(&self) -> Option<SumNetEvent>;
}

/// Production [`V2IngestRpc`] impl on the live `L1RpcClient`. Bridges
/// to the existing inherent methods so the pipeline (W10a) can use the
/// real chain endpoint with no glue at the call site.
#[async_trait]
impl V2IngestRpc for crate::rpc_client::L1RpcClient {
    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
        missing_offset: Option<u32>,
        missing_limit: Option<u32>,
    ) -> Result<AssignmentCoverageV2> {
        crate::rpc_client::L1RpcClient::storage_get_assignment_coverage_v2(
            self,
            merkle_root_hex,
            missing_offset,
            missing_limit,
        )
        .await
    }

    async fn chain_get_block_height(&self) -> Result<BlockHeightInfo> {
        crate::rpc_client::L1RpcClient::chain_get_block_height(self).await
    }

    async fn get_nonce(&self, addr_base58: &str) -> Result<u64> {
        crate::rpc_client::L1RpcClient::get_nonce(self, addr_base58).await
    }
}

/// Network stub used by chain-only operations (e.g. `abandon`) that
/// don't need libp2p. Lets `IngestPipeline` be constructed without
/// standing up a real swarm — useful when the operator just wants to
/// recover their fee deposit and shouldn't be blocked by local network
/// setup failures (e.g. UDP port in use, mDNS unavailable).
///
/// Both push methods bail loudly so a bug accidentally invoking them
/// from a non-abandon path surfaces immediately. `next_event` returns
/// `None` so any select loop terminates gracefully.
pub struct NoopNet;

#[async_trait]
impl V2IngestNet for NoopNet {
    async fn push_chunk_v2(
        &self,
        _peer_id: PeerId,
        _data: Vec<u8>,
        _merkle_root: [u8; 32],
        _chunk_index: u32,
        _merkle_path: Vec<[u8; 32]>,
    ) -> Result<()> {
        anyhow::bail!("NoopNet: push_chunk_v2 invoked from a chain-only pipeline (bug)")
    }
    async fn push_manifest_v2(
        &self,
        _peer_id: PeerId,
        _merkle_root: [u8; 32],
        _manifest_bytes: Vec<u8>,
    ) -> Result<()> {
        anyhow::bail!("NoopNet: push_manifest_v2 invoked from a chain-only pipeline (bug)")
    }
    async fn next_event(&self) -> Option<SumNetEvent> {
        None
    }
}

#[async_trait]
impl V2IngestNet for SumNet {
    async fn push_chunk_v2(
        &self,
        peer_id: PeerId,
        data: Vec<u8>,
        merkle_root: [u8; 32],
        chunk_index: u32,
        merkle_path: Vec<[u8; 32]>,
    ) -> Result<()> {
        SumNet::push_chunk_v2(self, peer_id, data, merkle_root, chunk_index, merkle_path).await
    }
    async fn push_manifest_v2(
        &self,
        peer_id: PeerId,
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
    ) -> Result<()> {
        SumNet::push_manifest_v2(self, peer_id, merkle_root, manifest_bytes).await
    }
    async fn next_event(&self) -> Option<SumNetEvent> {
        SumNet::next_event(self).await
    }
}

/// `[u8; 20]` archive address → libp2p `PeerId`. Production wraps the
/// existing `peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>`;
/// tests use a static map.
pub trait PeerResolver: Send + Sync {
    fn resolve(&self, addr: &[u8; 20]) -> Option<PeerId>;
}

/// Production [`PeerResolver`] backed by the same
/// `peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>` map V1's
/// upload path uses. Walks the map on each call (O(n), n = tens of
/// peers in Phase 0b) so newly-identified peers become resolvable
/// without rebuilding the resolver.
pub struct MapPeerResolver {
    peer_addresses: Arc<tokio::sync::RwLock<HashMap<PeerId, [u8; 20]>>>,
}

impl MapPeerResolver {
    pub fn new(
        peer_addresses: Arc<tokio::sync::RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> Self {
        Self { peer_addresses }
    }
}

impl PeerResolver for MapPeerResolver {
    fn resolve(&self, addr: &[u8; 20]) -> Option<PeerId> {
        // `try_read` rather than `read().await` so the trait can stay
        // sync. Under contention we fall back to "unknown peer", which
        // surfaces as an `unresolved_per_chunk` count → under-replicated.
        // The retry pass on the next assignment iteration finds the peer
        // when the lock is free. For Phase 0b's typical peer-discovery
        // pattern (peer map written once at startup, then read-mostly)
        // this is a non-issue.
        match self.peer_addresses.try_read() {
            Ok(map) => map
                .iter()
                .find_map(|(peer_id, l1)| if l1 == addr { Some(*peer_id) } else { None }),
            Err(_) => None,
        }
    }
}

// ── Pipeline ────────────────────────────────────────────────────────────────

pub struct IngestPipeline<R, N, P>
where
    R: V2IngestRpc + 'static,
    N: V2IngestNet + 'static,
    P: PeerResolver + 'static,
{
    rpc: Arc<R>,
    net: Arc<N>,
    peers: Arc<P>,
    signing_key_seed: [u8; 32],
    my_addr_base58: String,
    params: IngestParams,
}

impl<R, N, P> IngestPipeline<R, N, P>
where
    R: V2IngestRpc + 'static,
    N: V2IngestNet + 'static,
    P: PeerResolver + 'static,
{
    pub fn new(
        rpc: Arc<R>,
        net: Arc<N>,
        peers: Arc<P>,
        signing_key_seed: [u8; 32],
        my_addr: [u8; 20],
        params: IngestParams,
    ) -> Self {
        Self {
            rpc,
            net,
            peers,
            signing_key_seed,
            my_addr_base58: sum_net::l1_address_base58(&my_addr),
            params,
        }
    }

    /// Run the full S0–S6 pipeline against `path`.
    pub async fn run(&self, path: &Path) -> IngestOutcome {
        // ── S0 ─────────────────────────────────────────────────────
        let (manifest, mmap, tree) = match self.s0_chunk(path).await {
            Ok(triple) => triple,
            Err(e) => {
                return IngestOutcome::Failed {
                    stage: IngestStage::Register,
                    manifest: None,
                    source: e,
                };
            }
        };

        if manifest.chunk_count > self.params.max_chunk_count_per_file {
            return IngestOutcome::Failed {
                stage: IngestStage::Register,
                manifest: Some(manifest.clone()),
                source: anyhow::anyhow!(
                    "chunk_count {} exceeds max_chunk_count_per_file {}",
                    manifest.chunk_count,
                    self.params.max_chunk_count_per_file
                ),
            };
        }

        // ── S1 ─────────────────────────────────────────────────────
        let (register_tx_hash, register_height) = match self.s1_register_pending(&manifest).await {
            Ok(pair) => pair,
            Err(e) => {
                return IngestOutcome::Failed {
                    stage: IngestStage::Register,
                    manifest: Some(manifest),
                    source: e,
                };
            }
        };
        let merkle_root = manifest.merkle_root;

        // ── S2 ─────────────────────────────────────────────────────
        let (info, snapshot) = match self.fetch_assignment_inputs(&merkle_root).await {
            Ok(p) => p,
            Err(e) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Push,
                    last_coverage: None,
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }
        };

        // Full ingest: push every chunk. Resume narrows this to
        // missing_indices via `s2_push_chunks(..., &missing_set)`.
        let chunks_to_push: BTreeSet<u32> = (0..manifest.chunk_count).collect();
        let distinct_assigned = match self
            .s2_push_chunks(&manifest, &mmap, &tree, &info, &snapshot, &chunks_to_push)
            .await
        {
            Ok(distinct) => distinct,
            Err(under) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Push,
                    last_coverage: None,
                    under_replicated_chunks: Some(under.under_replicated),
                    suggested: SuggestedAction::Resume,
                    source: under.source,
                };
            }
        };

        // ── S3 ─────────────────────────────────────────────────────
        if let Err(e) = self.s3_push_manifest(&manifest, &distinct_assigned).await {
            return IngestOutcome::PendingNeedsAction {
                merkle_root,
                manifest,
                failed_stage: IngestStage::ManifestPush,
                last_coverage: None,
                under_replicated_chunks: None,
                suggested: SuggestedAction::Resume,
                source: Some(e),
            };
        }

        // ── S4 ─────────────────────────────────────────────────────
        let last_coverage = match self.s4_wait_coverage(&merkle_root).await {
            Ok(cov) => cov,
            Err(timeout) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Coverage,
                    last_coverage: timeout.last_coverage,
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: None,
                };
            }
        };

        // ── S5 ─────────────────────────────────────────────────────
        let (activate_tx_hash, activate_height) = match self.s5_activate(&merkle_root).await {
            Ok(pair) => pair,
            Err(e) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Activate,
                    last_coverage: Some(last_coverage),
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }
        };

        // ── S6 ─────────────────────────────────────────────────────
        info!(
            root = %hex::encode(merkle_root),
            register_height,
            activate_height,
            "V2 ingest activated"
        );
        IngestOutcome::Activated {
            merkle_root,
            manifest,
            register_tx_hash,
            register_height,
            activate_tx_hash,
            activate_height,
        }
    }

    // ── Stage helpers ────────────────────────────────────────────────

    async fn s0_chunk(&self, path: &Path) -> Result<(DataManifest, memmap2::Mmap, MerkleTree)> {
        let (mmap, manifest) = BinaryChunker::chunk_file(path)?;
        // Rebuild the tree once so we can serve `proof_bytes(i)` per
        // chunk during S2. `BinaryChunker::chunk_file` builds the tree
        // internally to compute `merkle_root` but doesn't surface it,
        // so we rebuild from the manifest's chunk hashes.
        let leaves: Vec<blake3::Hash> = manifest
            .chunks
            .iter()
            .map(|c| blake3::Hash::from(c.blake3_hash))
            .collect();
        let tree = MerkleTree::build(&leaves);
        debug_assert_eq!(*tree.root().as_bytes(), manifest.merkle_root);
        Ok((manifest, mmap, tree))
    }

    async fn s1_register_pending(
        &self,
        manifest: &DataManifest,
    ) -> Result<(String, u64)> {
        let nonce = self.rpc.get_nonce(&self.my_addr_base58).await?;
        let tx_hex = build_register_file_pending_v2_tx(
            &self.signing_key_seed,
            self.params.chain_id,
            nonce,
            self.params.fee_per_tx,
            manifest.merkle_root,
            manifest.total_size_bytes,
            manifest.total_size_bytes, // Public: stored == plaintext
            manifest.chunk_count,
            0, // fee_deposit; W10b CLI may parameterize
            0, // visibility = Public
            vec![], // empty initial_access (Public)
        )?;
        let tx_hash = self.rpc.send_raw_transaction(&tx_hex).await?;
        let height = wait_for_finalized(
            self.rpc.as_ref(),
            &tx_hash,
            self.params.poll_interval,
            self.params.finality_timeout,
        )
        .await
        .map_err(|e| match e {
            TxWaitError::Failed { reason, .. } => anyhow::anyhow!("RegisterFilePendingV2 failed: {reason}"),
            TxWaitError::Dropped => anyhow::anyhow!("RegisterFilePendingV2 dropped (resubmit with new nonce)"),
            TxWaitError::Timeout { last_status } => anyhow::anyhow!("RegisterFilePendingV2 timeout, last status: {last_status:?}"),
            TxWaitError::Rpc(e) => e,
        })?;
        info!(
            tx_hash,
            height,
            root = %hex::encode(manifest.merkle_root),
            "RegisterFilePendingV2 finalized"
        );
        Ok((tx_hash, height))
    }

    async fn fetch_assignment_inputs(
        &self,
        merkle_root: &[u8; 32],
    ) -> Result<(StorageFileInfoV2, Vec<[u8; 20]>)> {
        let root_hex = format!("0x{}", hex::encode(merkle_root));
        let info = self.rpc.storage_get_file_info_v2(&root_hex).await?;
        let raw = self
            .rpc
            .storage_get_active_nodes_at_height(info.assignment_height)
            .await?;
        let mut snapshot = Vec::with_capacity(raw.len());
        for record in &raw {
            let addr = sum_net::l1_address_from_base58(&record.address)?;
            snapshot.push(addr);
        }
        snapshot.sort();
        snapshot.dedup();
        Ok((info, snapshot))
    }

    /// Push chunks in `chunks_to_push` to their assigned archives.
    /// `chunks_to_push` is typically `(0..chunk_count)` for a full
    /// ingest or `coverage.missing_indices` for a resume.
    ///
    /// Returns the **full file's** distinct-assigned archive set (not
    /// just the subset assigned to `chunks_to_push`). The manifest
    /// re-push in S3 needs to reach every archive assigned to any
    /// chunk in `[0, chunk_count)` — those are the archives whose
    /// attestor is on the hook to eventually attest.
    async fn s2_push_chunks(
        &self,
        manifest: &DataManifest,
        mmap: &memmap2::Mmap,
        tree: &MerkleTree,
        _info: &StorageFileInfoV2,
        snapshot: &[[u8; 20]],
        chunks_to_push: &BTreeSet<u32>,
    ) -> Result<BTreeSet<[u8; 20]>, PushFailure> {
        let r = self.params.assignment_replication_factor;
        // Build (chunk_index, peer_id) targets ONLY for the subset
        // we're pushing. Track peers we couldn't resolve up front —
        // those become "permanently un-pushable" and contribute to
        // under-replication. Build the full-file distinct_assigned set
        // in the same pass so the caller can hand it to S3.
        let mut targets: Vec<(u32, PeerId)> = Vec::new();
        let mut unresolved_per_chunk: HashMap<u32, u32> = HashMap::new();
        let mut distinct_assigned: BTreeSet<[u8; 20]> = BTreeSet::new();
        for chunk_index in 0..manifest.chunk_count {
            let assigned = assigned_archives(&manifest.merkle_root, snapshot, chunk_index, r);
            for archive in &assigned {
                distinct_assigned.insert(*archive);
            }
            if !chunks_to_push.contains(&chunk_index) {
                continue;
            }
            for archive in &assigned {
                match self.peers.resolve(archive) {
                    Some(peer) => targets.push((chunk_index, peer)),
                    None => *unresolved_per_chunk.entry(chunk_index).or_default() += 1,
                }
            }
        }

        // Per-(chunk, peer) attempt counter; capped at push_retries + 1.
        // Exactly mirrors the planned target set — a `(chunk, peer)`
        // tuple NOT in `attempts` is by definition a non-target.
        let mut attempts: HashMap<(u32, PeerId), u32> =
            targets.iter().map(|k| (*k, 0u32)).collect();
        let mut acked_per_chunk: HashMap<u32, HashSet<PeerId>> = HashMap::new();

        // Initial push wave. A push send failure here doesn't immediately
        // abort — we record the (chunk, peer) pair as "no attempt yet" so
        // it can still be picked up by the retry loop on a later event.
        for (chunk_index, peer) in &targets {
            match self.send_one_push(manifest, mmap, tree, *chunk_index, *peer).await {
                Ok(()) => {
                    *attempts.get_mut(&(*chunk_index, *peer)).unwrap() = 1;
                }
                Err(e) => {
                    warn!(chunk_index, ?peer, %e, "S2 initial push send failed");
                    // Leave attempts[(chunk, peer)] = 0 so retries can fire
                    // later if the local network recovers within the window.
                }
            }
        }

        let deadline = Instant::now() + self.params.push_wait_secs;
        loop {
            // Early-exit: every chunk we asked to push has reached R
            // distinct successful ACKs. This is scoped to
            // `chunks_to_push`, NOT to `[0, chunk_count)` — for resume
            // we may only be retrying a few chunks, and we MUST exit
            // S2 the moment those finish so subsequent events
            // (ManifestPushAcks queued for S3) aren't drained here.
            if chunks_replicated_in(&acked_per_chunk, chunks_to_push, r) {
                return Ok(distinct_assigned);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let event = tokio::select! {
                ev = self.net.next_event() => ev,
                _ = tokio::time::sleep(remaining) => break,
            };
            let Some(ev) = event else { break };
            match ev {
                SumNetEvent::ShardReceivedV2 {
                    peer_id,
                    response: ShardResponseV2::PushAck { merkle_root, chunk_index, error },
                } if merkle_root == manifest.merkle_root => {
                    let key = (chunk_index, peer_id);
                    // Reject ACKs from peers we never targeted for this
                    // chunk. Without this gate, an unassigned (or
                    // spoofed/stale) peer could falsely satisfy the R
                    // threshold; the retry branch could also send to a
                    // non-target peer.
                    if !attempts.contains_key(&key) {
                        debug!(
                            chunk_index,
                            ?peer_id,
                            "S2 ignoring PushAck from untargeted (chunk, peer)"
                        );
                        continue;
                    }
                    if error.is_none() {
                        // Insert into the per-chunk peer set. HashSet
                        // ensures duplicate ACKs from the same peer
                        // don't double-count toward R.
                        acked_per_chunk
                            .entry(chunk_index)
                            .or_default()
                            .insert(peer_id);
                    } else {
                        // Safe-by-construction: `attempts.contains_key`
                        // gate above guarantees `get_mut` returns Some.
                        let attempt = attempts
                            .get_mut(&key)
                            .expect("checked by attempts.contains_key gate above");
                        if *attempt <= self.params.push_retries {
                            *attempt += 1;
                            if let Err(e) = self
                                .send_one_push(manifest, mmap, tree, chunk_index, peer_id)
                                .await
                            {
                                warn!(?key, %e, "S2 retry send failed");
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Final under-replication audit — only audit chunks we
        // actually attempted to push. For resume, that's the missing
        // set; for full ingest, that's [0, chunk_count).
        let mut under: Vec<u32> = chunks_to_push
            .iter()
            .copied()
            .filter(|i| {
                acked_per_chunk
                    .get(i)
                    .map(|set| (set.len() as u32) < r)
                    .unwrap_or(true)
            })
            .collect();
        under.sort();
        if under.is_empty() {
            Ok(distinct_assigned)
        } else {
            warn!(count = under.len(), "S2 under-replicated chunks");
            Err(PushFailure {
                under_replicated: under,
                source: None,
            })
        }
    }

    async fn send_one_push(
        &self,
        manifest: &DataManifest,
        mmap: &memmap2::Mmap,
        tree: &MerkleTree,
        chunk_index: u32,
        peer_id: PeerId,
    ) -> Result<()> {
        let cd = &manifest.chunks[chunk_index as usize];
        let start = cd.offset as usize;
        let end = start + cd.size as usize;
        let data = mmap[start..end].to_vec();
        let proof = tree.proof_bytes(chunk_index);
        self.net
            .push_chunk_v2(peer_id, data, manifest.merkle_root, chunk_index, proof)
            .await
    }

    async fn s3_push_manifest(
        &self,
        manifest: &DataManifest,
        distinct_assigned: &BTreeSet<[u8; 20]>,
    ) -> Result<()> {
        // Push the manifest to **exactly the set of archives the
        // per-chunk V2 assignment landed on** in S2. Sending to
        // unrelated snapshot peers (the rejected v1 W10a behavior)
        // bloats traffic and could fail S3 if a non-assigned snapshot
        // peer never ACKs — its V2 attestor wouldn't fire anyway since
        // its `held ∩ my_assignment` intersection is empty.
        let mut targets: Vec<PeerId> = distinct_assigned
            .iter()
            .filter_map(|addr| self.peers.resolve(addr))
            .collect();
        targets.sort();
        targets.dedup();
        let needed: HashSet<PeerId> = targets.iter().copied().collect();

        // Serialize manifest once.
        let mut manifest_bytes = Vec::new();
        ciborium::ser::into_writer(manifest, &mut manifest_bytes)?;

        for peer in &targets {
            self.net
                .push_manifest_v2(*peer, manifest.merkle_root, manifest_bytes.clone())
                .await?;
        }

        let mut acked: HashSet<PeerId> = HashSet::new();
        let deadline = Instant::now() + self.params.manifest_push_wait_secs;
        while acked.len() < needed.len() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let event = tokio::select! {
                ev = self.net.next_event() => ev,
                _ = tokio::time::sleep(remaining) => break,
            };
            let Some(ev) = event else { break };
            if let SumNetEvent::ShardReceivedV2 {
                peer_id,
                response: ShardResponseV2::ManifestPushAck { merkle_root, error },
            } = ev
            {
                if merkle_root == manifest.merkle_root && error.is_none() && needed.contains(&peer_id) {
                    acked.insert(peer_id);
                }
            }
        }

        if acked.len() < needed.len() {
            anyhow::bail!(
                "ManifestPush: only {} of {} assigned-archive targets ACKed",
                acked.len(),
                needed.len()
            );
        }
        Ok(())
    }

    async fn s4_wait_coverage(
        &self,
        merkle_root: &[u8; 32],
    ) -> Result<AssignmentCoverageV2, CoverageTimeout> {
        let root_hex = format!("0x{}", hex::encode(merkle_root));
        let deadline = Instant::now() + self.params.activation_wait_secs;
        let mut last: Option<AssignmentCoverageV2> = None;
        loop {
            match self
                .rpc
                .storage_get_assignment_coverage_v2(&root_hex, None, None)
                .await
            {
                Ok(cov) => {
                    last = Some(cov.clone());
                    if cov.can_activate_now {
                        debug!(
                            root = %root_hex,
                            covered = cov.covered_count,
                            chunks = cov.chunk_count,
                            "S4 coverage: can_activate_now=true"
                        );
                        return Ok(cov);
                    }
                }
                Err(e) => {
                    warn!(%e, "S4 coverage poll RPC error — retrying");
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(CoverageTimeout { last_coverage: last });
            }
            let sleep_for = std::cmp::min(self.params.poll_interval, deadline - now);
            tokio::time::sleep(sleep_for).await;
        }
    }

    async fn s5_activate(&self, merkle_root: &[u8; 32]) -> Result<(String, u64)> {
        let nonce = self.rpc.get_nonce(&self.my_addr_base58).await?;
        let tx_hex = build_activate_file_v2_tx(
            &self.signing_key_seed,
            self.params.chain_id,
            nonce,
            self.params.fee_per_tx,
            *merkle_root,
        )?;
        let tx_hash = self.rpc.send_raw_transaction(&tx_hex).await?;
        let height = wait_for_finalized(
            self.rpc.as_ref(),
            &tx_hash,
            self.params.poll_interval,
            self.params.finality_timeout,
        )
        .await
        .map_err(|e| match e {
            TxWaitError::Failed { reason, .. } => anyhow::anyhow!("ActivateFileV2 failed: {reason}"),
            TxWaitError::Dropped => anyhow::anyhow!("ActivateFileV2 dropped"),
            TxWaitError::Timeout { last_status } => {
                anyhow::anyhow!("ActivateFileV2 timeout, last status: {last_status:?}")
            }
            TxWaitError::Rpc(e) => e,
        })?;
        Ok((tx_hash, height))
    }

    // ── Resume (W11) ────────────────────────────────────────────────
    //
    // Re-runs the post-register portion of the V2 lifecycle against a
    // file already known to be `Pending` on chain. The caller passes
    // both the explicit `merkle_root` (recorded from the prior
    // `PendingNeedsAction` outcome) AND the file path (so we can
    // re-chunk and rebuild Merkle proofs for any missing pushes).
    //
    // Mismatch between caller's `merkle_root` and the path's computed
    // root is a typed error — operators usually trip this by passing
    // the wrong file for the recorded root. Never auto-recoverable.

    pub async fn resume(
        &self,
        merkle_root: [u8; 32],
        file_path: &Path,
    ) -> IngestOutcome {
        // Re-chunk and verify the path matches the recorded root.
        let (manifest, mmap, tree) = match self.s0_chunk(file_path).await {
            Ok(triple) => triple,
            Err(e) => {
                return IngestOutcome::Failed {
                    stage: IngestStage::Register,
                    manifest: None,
                    source: e,
                };
            }
        };
        if manifest.merkle_root != merkle_root {
            return IngestOutcome::RootMismatch {
                expected: merkle_root,
                actual: manifest.merkle_root,
                manifest,
            };
        }

        // Lifecycle gate.
        let info = match self
            .rpc
            .storage_get_file_info_v2(&format!("0x{}", hex::encode(merkle_root)))
            .await
        {
            Ok(info) => info,
            Err(e) => {
                return IngestOutcome::Failed {
                    stage: IngestStage::Register,
                    manifest: Some(manifest),
                    source: e.context("resume: storage_getFileInfoV2 failed"),
                };
            }
        };
        if info.lifecycle.is_active() {
            // No work to do. Heights from chain state (no tx hashes —
            // chain plan §4 doesn't expose them).
            //
            // `activated_at_height = None` while `lifecycle == Active`
            // is chain-shape corruption: chain plan §3.2 sets
            // `activated_at_height = Some(block_height)` as part of
            // the same `ActivateFileV2` execution that flips lifecycle.
            // Surface explicitly rather than silently reporting height 0.
            let activate_height = match info.activated_at_height {
                Some(h) => h,
                None => {
                    return IngestOutcome::Failed {
                        stage: IngestStage::Register,
                        manifest: Some(manifest),
                        source: anyhow::anyhow!(
                            "chain shape corruption: lifecycle=Active but \
                             activated_at_height is None for root 0x{}",
                            hex::encode(merkle_root),
                        ),
                    };
                }
            };
            return IngestOutcome::ActivatedOnChain {
                merkle_root,
                manifest,
                register_height: info.created_at,
                activate_height,
            };
        }
        if info.lifecycle.is_abandoned() {
            return IngestOutcome::AbandonedOnChain {
                merkle_root,
                manifest,
                // Chain v3.3+ surfaces this; pre-v3.3 returns None.
                // Defensively passed through — operator guidance only.
                abandoned_at_height: info.abandoned_at_height,
            };
        }
        if !info.lifecycle.is_pending() {
            return IngestOutcome::Failed {
                stage: IngestStage::Register,
                manifest: Some(manifest),
                source: anyhow::anyhow!(
                    "resume: unexpected lifecycle byte {} on chain (expected Pending/Active/Abandoned)",
                    info.lifecycle.0,
                ),
            };
        }

        // Snapshot. Same as W10a.
        let snapshot = match self
            .rpc
            .storage_get_active_nodes_at_height(info.assignment_height)
            .await
        {
            Ok(raw) => {
                let mut snap = Vec::with_capacity(raw.len());
                for record in &raw {
                    match sum_net::l1_address_from_base58(&record.address) {
                        Ok(addr) => snap.push(addr),
                        Err(e) => {
                            return IngestOutcome::PendingNeedsAction {
                                merkle_root,
                                manifest,
                                failed_stage: IngestStage::Push,
                                last_coverage: None,
                                under_replicated_chunks: None,
                                suggested: SuggestedAction::Resume,
                                source: Some(e.context("snapshot address decode")),
                            };
                        }
                    }
                }
                snap.sort();
                snap.dedup();
                snap
            }
            Err(e) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Push,
                    last_coverage: None,
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }
        };

        // Coverage probe — drives whether to skip S2/S3 entirely.
        let coverage = match self
            .rpc
            .storage_get_assignment_coverage_v2(
                &format!("0x{}", hex::encode(merkle_root)),
                None,
                None,
            )
            .await
        {
            Ok(cov) => cov,
            Err(e) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Coverage,
                    last_coverage: None,
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }
        };

        // If the chain already says we can activate, skip S2/S3 and
        // jump straight to S5.
        let last_coverage = if coverage.can_activate_now {
            debug!(
                root = %hex::encode(merkle_root),
                "resume: can_activate_now=true, jumping to S5"
            );
            coverage
        } else {
            // Run a partial S2 over the missing chunk indices, then
            // re-run S3 (manifest is idempotent on receiver), then S4
            // wait for coverage, then continue to S5.
            //
            // Note we trust `coverage.missing_indices` as authoritative;
            // pagination is collected via missing_offset until empty.
            let missing = match self.collect_missing_indices(merkle_root).await {
                Ok(m) => m,
                Err(e) => {
                    return IngestOutcome::PendingNeedsAction {
                        merkle_root,
                        manifest,
                        failed_stage: IngestStage::Coverage,
                        last_coverage: Some(coverage),
                        under_replicated_chunks: None,
                        suggested: SuggestedAction::Resume,
                        source: Some(e),
                    };
                }
            };

            let distinct_assigned = match self
                .s2_push_chunks(&manifest, &mmap, &tree, &info, &snapshot, &missing)
                .await
            {
                Ok(distinct) => distinct,
                Err(under) => {
                    return IngestOutcome::PendingNeedsAction {
                        merkle_root,
                        manifest,
                        failed_stage: IngestStage::Push,
                        last_coverage: Some(coverage),
                        under_replicated_chunks: Some(under.under_replicated),
                        suggested: SuggestedAction::Resume,
                        source: under.source,
                    };
                }
            };

            // Re-push manifest to all distinct-assigned archives (idempotent).
            if let Err(e) = self.s3_push_manifest(&manifest, &distinct_assigned).await {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::ManifestPush,
                    last_coverage: Some(coverage),
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }

            // S4 wait — same wall-clock budget as a fresh ingest.
            match self.s4_wait_coverage(&merkle_root).await {
                Ok(cov) => cov,
                Err(timeout) => {
                    return IngestOutcome::PendingNeedsAction {
                        merkle_root,
                        manifest,
                        failed_stage: IngestStage::Coverage,
                        last_coverage: timeout.last_coverage,
                        under_replicated_chunks: None,
                        suggested: SuggestedAction::Resume,
                        source: None,
                    };
                }
            }
        };

        // S5 activate.
        let (activate_tx_hash, activate_height) = match self.s5_activate(&merkle_root).await {
            Ok(pair) => pair,
            Err(e) => {
                return IngestOutcome::PendingNeedsAction {
                    merkle_root,
                    manifest,
                    failed_stage: IngestStage::Activate,
                    last_coverage: Some(last_coverage),
                    under_replicated_chunks: None,
                    suggested: SuggestedAction::Resume,
                    source: Some(e),
                };
            }
        };

        // We have an activate_tx_hash from our own submit; the original
        // register_tx_hash is NOT in chain state (§4) and we never
        // submitted it ourselves. `ResumedActivated` is the typed
        // resume-success shape — it omits register_tx_hash entirely
        // rather than inventing one.
        info!(
            root = %hex::encode(merkle_root),
            register_height = info.created_at,
            activate_height,
            "resume: ActivateFileV2 finalized"
        );
        IngestOutcome::ResumedActivated {
            merkle_root,
            manifest,
            register_height: info.created_at,
            activate_tx_hash,
            activate_height,
        }
    }

    /// Walk paginated `missing_indices` until `missing_indices` is empty.
    /// Pagination cycles `missing_offset = last_returned_index + 1` per
    /// chain plan v3.2 §4 line 474.
    async fn collect_missing_indices(
        &self,
        merkle_root: [u8; 32],
    ) -> Result<BTreeSet<u32>> {
        let root_hex = format!("0x{}", hex::encode(merkle_root));
        let mut missing: BTreeSet<u32> = BTreeSet::new();
        let mut offset: u32 = 0;
        loop {
            let cov = self
                .rpc
                .storage_get_assignment_coverage_v2(&root_hex, Some(offset), None)
                .await?;
            if cov.missing_indices.is_empty() {
                break;
            }
            let last = *cov.missing_indices.last().unwrap();
            for idx in cov.missing_indices {
                missing.insert(idx);
            }
            // Advance past the highest index this page returned.
            // Saturating add guards against u32::MAX edge case.
            offset = last.saturating_add(1);
            // If chain returned the same final index, break to avoid
            // infinite loop on a buggy chain response.
            if offset == last {
                break;
            }
        }
        Ok(missing)
    }

    // ── Abandon (W11) ───────────────────────────────────────────────
    //
    // Submits `AbandonFileV2` for a `Pending` file we own. Pre-checks
    // chain state to give a clear "wait until height N" message
    // before burning a tx fee against chain plan §3.5 strict-`>` rule:
    //
    //   `current_height > created_at + activation_grace_blocks`
    //
    // → earliest_admissible_height = created_at + grace + 1.

    pub async fn abandon(&self, merkle_root: [u8; 32]) -> AbandonOutcome {
        let root_hex = format!("0x{}", hex::encode(merkle_root));
        let info = match self.rpc.storage_get_file_info_v2(&root_hex).await {
            Ok(info) => info,
            Err(e) => return AbandonOutcome::Failed { source: e },
        };
        let current_height = match self.rpc.chain_get_block_height().await {
            Ok(h) => h.height,
            Err(e) => return AbandonOutcome::Failed { source: e },
        };

        // Lifecycle gate. AbandonFileV2 is only valid for Pending
        // (chain plan §3.5).
        if !info.lifecycle.is_pending() {
            return AbandonOutcome::NotAdmissible {
                reason: format!(
                    "lifecycle byte {} (expected Pending = {})",
                    info.lifecycle.0,
                    sum_types::rpc_types::LifecycleV2::PENDING.0,
                ),
                current_height,
                // earliest_admissible isn't applicable when the lifecycle
                // is wrong; surface 0 to make that clear in tooling.
                earliest_admissible_height: 0,
            };
        }

        // Strict-greater grace check per chain plan §3.5:
        //   current_height > info.created_at + activation_grace_blocks
        // ⇔ current_height >= info.created_at + grace + 1
        let earliest = info
            .created_at
            .saturating_add(self.params.activation_grace_blocks)
            .saturating_add(1);
        if current_height < earliest {
            return AbandonOutcome::NotAdmissible {
                reason: format!(
                    "activation grace not yet expired (chain plan §3.5: \
                     current_height must be > created_at + activation_grace_blocks; \
                     created_at={}, grace={})",
                    info.created_at, self.params.activation_grace_blocks
                ),
                current_height,
                earliest_admissible_height: earliest,
            };
        }

        // Pre-check passed — submit.
        let nonce = match self.rpc.get_nonce(&self.my_addr_base58).await {
            Ok(n) => n,
            Err(e) => return AbandonOutcome::Failed { source: e },
        };
        let tx_hex = match crate::tx_builder::build_abandon_file_v2_tx(
            &self.signing_key_seed,
            self.params.chain_id,
            nonce,
            self.params.fee_per_tx,
            merkle_root,
        ) {
            Ok(s) => s,
            Err(e) => return AbandonOutcome::Failed { source: e },
        };
        let tx_hash = match self.rpc.send_raw_transaction(&tx_hex).await {
            Ok(h) => h,
            Err(e) => return AbandonOutcome::Failed { source: e },
        };
        let height = match wait_for_finalized(
            self.rpc.as_ref(),
            &tx_hash,
            self.params.poll_interval,
            self.params.finality_timeout,
        )
        .await
        {
            Ok(h) => h,
            Err(e) => match e {
                TxWaitError::Failed { reason, .. } => {
                    return AbandonOutcome::Failed {
                        source: anyhow::anyhow!("AbandonFileV2 failed: {reason}"),
                    };
                }
                TxWaitError::Dropped => {
                    return AbandonOutcome::Failed {
                        source: anyhow::anyhow!("AbandonFileV2 dropped (resubmit with new nonce)"),
                    };
                }
                TxWaitError::Timeout { last_status } => {
                    return AbandonOutcome::Failed {
                        source: anyhow::anyhow!(
                            "AbandonFileV2 timeout; last status: {last_status:?}"
                        ),
                    };
                }
                TxWaitError::Rpc(e) => return AbandonOutcome::Failed { source: e },
            },
        };
        info!(
            root = %hex::encode(merkle_root),
            %tx_hash,
            finalized_at_height = height,
            "AbandonFileV2 finalized"
        );
        AbandonOutcome::Abandoned {
            tx_hash,
            finalized_at_height: height,
        }
    }
}

#[derive(Debug)]
struct PushFailure {
    under_replicated: Vec<u32>,
    source: Option<anyhow::Error>,
}

#[derive(Debug)]
struct CoverageTimeout {
    last_coverage: Option<AssignmentCoverageV2>,
}

/// Replication check scoped to a specific subset of chunk indices.
/// Replaces the previous all-chunks variant — for full ingest the
/// caller passes `(0..chunk_count).collect()`, for resume just the
/// missing-indices set.
fn chunks_replicated_in(
    acks: &HashMap<u32, HashSet<PeerId>>,
    chunks: &BTreeSet<u32>,
    replication_factor: u32,
) -> bool {
    chunks.iter().all(|i| {
        acks.get(i)
            .map(|set| (set.len() as u32) >= replication_factor)
            .unwrap_or(false)
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use sum_net::l1_address_base58;
    use sum_types::rpc_types::{LifecycleV2, NodeRecordInfo, TxStatusV2, VisibilityV2};
    use crate::tx_wait::TxStatusSource;

    /// In-memory RPC mock for the W10a state machine. All methods queue
    /// canned responses; tests assert on call counts and side effects.
    #[derive(Default)]
    struct MockRpc {
        files: StdMutex<HashMap<String, StorageFileInfoV2>>,
        snapshots: StdMutex<HashMap<u64, Vec<NodeRecordInfo>>>,
        coverages: StdMutex<VecDeque<Result<AssignmentCoverageV2, String>>>,
        statuses: StdMutex<VecDeque<Result<TxStatusV2, String>>>,
        sends: StdMutex<VecDeque<Result<String, String>>>,
        nonces: StdMutex<HashMap<String, u64>>,
        sent_txs: StdMutex<Vec<String>>,
        coverage_polls: StdMutex<u32>,
    }

    impl MockRpc {
        fn add_file(&self, root_hex: &str, info: StorageFileInfoV2) {
            self.files.lock().unwrap().insert(root_hex.into(), info);
        }
        fn add_snapshot(&self, height: u64, nodes: Vec<NodeRecordInfo>) {
            self.snapshots.lock().unwrap().insert(height, nodes);
        }
        fn enqueue_coverage(&self, cov: AssignmentCoverageV2) {
            self.coverages.lock().unwrap().push_back(Ok(cov));
        }
        fn enqueue_status(&self, st: TxStatusV2) {
            self.statuses.lock().unwrap().push_back(Ok(st));
        }
        fn enqueue_send(&self, hash: &str) {
            self.sends.lock().unwrap().push_back(Ok(hash.into()));
        }
        fn enqueue_send_err(&self, msg: &str) {
            self.sends.lock().unwrap().push_back(Err(msg.into()));
        }
        fn set_nonce(&self, addr: &str, n: u64) {
            self.nonces.lock().unwrap().insert(addr.into(), n);
        }
        fn coverage_poll_count(&self) -> u32 {
            *self.coverage_polls.lock().unwrap()
        }
    }

    #[async_trait]
    impl V2RpcClient for MockRpc {
        async fn storage_get_file_info_v2(
            &self,
            merkle_root_hex: &str,
        ) -> Result<StorageFileInfoV2> {
            self.files
                .lock()
                .unwrap()
                .get(merkle_root_hex)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown root: {merkle_root_hex}"))
        }
        async fn storage_get_active_nodes_at_height(
            &self,
            height: u64,
        ) -> Result<Vec<NodeRecordInfo>> {
            self.snapshots
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no snapshot at height {height}"))
        }
    }

    #[async_trait]
    impl TxStatusSource for MockRpc {
        async fn get_transaction_status(&self, _tx_hash: &str) -> Result<TxStatusV2> {
            let next = self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no status response queued"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    #[async_trait]
    impl AttestorRpc for MockRpc {
        async fn send_raw_transaction(&self, hex: &str) -> Result<String> {
            self.sent_txs.lock().unwrap().push(hex.into());
            let next = self
                .sends
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no send response queued"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    #[async_trait]
    impl V2IngestRpc for MockRpc {
        async fn storage_get_assignment_coverage_v2(
            &self,
            _merkle_root_hex: &str,
            _missing_offset: Option<u32>,
            _missing_limit: Option<u32>,
        ) -> Result<AssignmentCoverageV2> {
            *self.coverage_polls.lock().unwrap() += 1;
            let next = self
                .coverages
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no coverage response queued"))?;
            next.map_err(anyhow::Error::msg)
        }
        async fn chain_get_block_height(&self) -> Result<BlockHeightInfo> {
            Ok(BlockHeightInfo {
                height: 1000,
                finality: "finalized".into(),
            })
        }
        async fn get_nonce(&self, addr_base58: &str) -> Result<u64> {
            self.nonces
                .lock()
                .unwrap()
                .get(addr_base58)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("no nonce for {addr_base58}"))
        }
    }

    /// Mock V2 network. Records pushes; events delivered via a queue
    /// the test fills explicitly to mimic ack arrivals.
    #[derive(Default)]
    struct MockNet {
        pushes: StdMutex<Vec<(PeerId, [u8; 32], u32)>>,
        manifest_pushes: StdMutex<Vec<(PeerId, [u8; 32])>>,
        events: tokio::sync::Mutex<VecDeque<SumNetEvent>>,
        push_chunk_results: StdMutex<VecDeque<Result<(), String>>>,
        push_manifest_results: StdMutex<VecDeque<Result<(), String>>>,
    }

    impl MockNet {
        fn new() -> Self {
            Self::default()
        }
        async fn push_event(&self, ev: SumNetEvent) {
            self.events.lock().await.push_back(ev);
        }
        fn push_count(&self) -> usize {
            self.pushes.lock().unwrap().len()
        }
        fn manifest_push_count(&self) -> usize {
            self.manifest_pushes.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl V2IngestNet for MockNet {
        async fn push_chunk_v2(
            &self,
            peer_id: PeerId,
            _data: Vec<u8>,
            merkle_root: [u8; 32],
            chunk_index: u32,
            _merkle_path: Vec<[u8; 32]>,
        ) -> Result<()> {
            self.pushes.lock().unwrap().push((peer_id, merkle_root, chunk_index));
            let next = self.push_chunk_results.lock().unwrap().pop_front();
            match next {
                None | Some(Ok(())) => Ok(()),
                Some(Err(s)) => Err(anyhow::anyhow!(s)),
            }
        }
        async fn push_manifest_v2(
            &self,
            peer_id: PeerId,
            merkle_root: [u8; 32],
            _manifest_bytes: Vec<u8>,
        ) -> Result<()> {
            self.manifest_pushes.lock().unwrap().push((peer_id, merkle_root));
            let next = self.push_manifest_results.lock().unwrap().pop_front();
            match next {
                None | Some(Ok(())) => Ok(()),
                Some(Err(s)) => Err(anyhow::anyhow!(s)),
            }
        }
        async fn next_event(&self) -> Option<SumNetEvent> {
            self.events.lock().await.pop_front()
        }
    }

    /// Static address→peer mapping.
    struct StaticPeers {
        map: HashMap<[u8; 20], PeerId>,
    }

    impl PeerResolver for StaticPeers {
        fn resolve(&self, addr: &[u8; 20]) -> Option<PeerId> {
            self.map.get(addr).copied()
        }
    }

    fn fake_peer() -> PeerId {
        sum_net::Keypair::generate_ed25519().public().to_peer_id()
    }

    fn five_archives() -> Vec<[u8; 20]> {
        (0..5)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xAA + i;
                a
            })
            .collect()
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

    fn pending_file_info(root: &[u8; 32], chunk_count: u32, height: u64) -> StorageFileInfoV2 {
        StorageFileInfoV2 {
            merkle_root: format!("0x{}", hex::encode(root)),
            owner: l1_address_base58(&[0x01; 20]),
            plaintext_size_bytes: 1024,
            stored_size_bytes: 1024,
            chunk_count,
            fee_pool: 1000,
            created_at: 100,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: height,
            visibility: VisibilityV2::PUBLIC,
            lifecycle: LifecycleV2::PENDING,
            access_list: vec![],
        }
    }

    fn coverage_active(chunk_count: u32, can_now: bool) -> AssignmentCoverageV2 {
        AssignmentCoverageV2 {
            chunk_count,
            covered_count: if can_now { chunk_count } else { 0 },
            can_activate_now: can_now,
            missing_total: if can_now { 0 } else { chunk_count },
            missing_offset: 0,
            missing_indices: vec![],
            per_archive: vec![],
        }
    }

    fn write_test_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ingest.bin");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn defaults_for_tests() -> IngestParamsDefaults {
        IngestParamsDefaults {
            poll_interval: Duration::from_millis(10),
            activation_wait_secs: Duration::from_millis(500),
            finality_timeout: Duration::from_secs(2),
            push_retries: 2,
            push_wait_secs: Duration::from_millis(500),
            manifest_push_wait_secs: Duration::from_millis(500),
        }
    }

    fn params_for_test(d: IngestParamsDefaults) -> IngestParams {
        IngestParams {
            assignment_replication_factor: 5,
            max_chunk_indices_per_tx: 65_536,
            max_chunk_count_per_file: 1_048_576,
            chain_id: 31337,
            fee_per_tx: 1_000,
            poll_interval: d.poll_interval,
            activation_wait_secs: d.activation_wait_secs,
            finality_timeout: d.finality_timeout,
            push_retries: d.push_retries,
            push_wait_secs: d.push_wait_secs,
            manifest_push_wait_secs: d.manifest_push_wait_secs,
            activation_grace_blocks: 50,
        }
    }

    /// Spin up a fully wired pipeline with R=5 (every archive in our
    /// 5-archive snapshot is assigned to every chunk, so test peer
    /// resolution is total).
    fn build_pipeline(
        rpc: Arc<MockRpc>,
        net: Arc<MockNet>,
        archive_to_peer: HashMap<[u8; 20], PeerId>,
        my_addr: [u8; 20],
        params: IngestParams,
    ) -> IngestPipeline<MockRpc, MockNet, StaticPeers> {
        IngestPipeline::new(
            rpc,
            net,
            Arc::new(StaticPeers { map: archive_to_peer }),
            [42u8; 32],
            my_addr,
            params,
        )
    }

    /// Helper: run the pipeline and feed scripted PushAck/ManifestPushAck
    /// events to drive S2/S3.
    async fn ack_chunks_for_all(
        net: &MockNet,
        merkle_root: [u8; 32],
        chunk_count: u32,
        peers: &[PeerId],
    ) {
        for chunk_index in 0..chunk_count {
            for peer_id in peers {
                net.push_event(SumNetEvent::ShardReceivedV2 {
                    peer_id: *peer_id,
                    response: ShardResponseV2::PushAck {
                        merkle_root,
                        chunk_index,
                        error: None,
                    },
                })
                .await;
            }
        }
    }

    async fn ack_manifest_for_all(net: &MockNet, merkle_root: [u8; 32], peers: &[PeerId]) {
        for peer_id in peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: None,
                },
            })
            .await;
        }
    }

    // ── Tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_s0_through_s6_activates() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        // Make a 4-chunk file (chunk size is 1MiB; we use 4 MiB).
        let bytes = vec![0xCDu8; 4 * 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        // We don't know the merkle_root until S0 chunks the file. Build
        // a pipeline that has ample scripted responses and let it figure
        // out the root.
        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        // S1: send ok, finalized.
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        // S5: send ok, finalized.
        rpc.enqueue_send("0xtx-activate");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 200 });
        // Coverage: returns can_activate_now = true on first poll.
        rpc.enqueue_coverage(coverage_active(4, true));

        let net = Arc::new(MockNet::new());

        // Compute the merkle root the pipeline will produce. We do a
        // dry-run by chunking the same file and reading its manifest.
        let (_mmap_dryrun, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        // After we know the root, we can register the file_info + snapshot.
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, manifest_dryrun.chunk_count, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        // Pre-queue PushAck + ManifestPushAck events for every chunk × peer.
        ack_chunks_for_all(&net, merkle_root, manifest_dryrun.chunk_count, &peers).await;
        ack_manifest_for_all(&net, merkle_root, &peers).await;

        let pipeline = build_pipeline(
            rpc.clone(),
            net.clone(),
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        let outcome = pipeline.run(&path).await;
        match outcome {
            IngestOutcome::Activated {
                merkle_root: r,
                register_height,
                activate_height,
                ..
            } => {
                assert_eq!(r, merkle_root);
                assert_eq!(register_height, 100);
                assert_eq!(activate_height, 200);
            }
            other => panic!("expected Activated, got {other:?}"),
        }
        // Sanity: 4 chunks × 5 peers = 20 pushes; 5 manifest pushes.
        assert_eq!(net.push_count(), 20);
        assert_eq!(net.manifest_push_count(), 5);
    }

    #[tokio::test]
    async fn s1_register_failure_returns_failed_with_no_chain_state() {
        let bytes = vec![0xAB; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 5);
        rpc.enqueue_send_err("RegisterFilePendingV2 rejected: visibility/bundle mismatch");
        let net = Arc::new(MockNet::new());

        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        let outcome = pipeline.run(&path).await;
        match outcome {
            IngestOutcome::Failed { stage, manifest, source } => {
                assert_eq!(stage, IngestStage::Register);
                assert!(manifest.is_some(), "manifest must be preserved across S1 failure");
                assert!(source.to_string().contains("visibility/bundle mismatch"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s2_chunk_under_replicated_returns_pending_needs_action() {
        let bytes = vec![0xEF; 2 * 1_048_576]; // 2 chunks
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, manifest_dryrun.chunk_count, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        // Only ACK chunk 0 from all 5 peers; chunk 1 gets nothing → under-replicated.
        for peer_id in &peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }

        let pipeline = build_pipeline(
            rpc,
            net,
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.run(&path).await {
            IngestOutcome::PendingNeedsAction {
                merkle_root: r,
                failed_stage,
                under_replicated_chunks,
                suggested,
                ..
            } => {
                assert_eq!(r, merkle_root);
                assert_eq!(failed_stage, IngestStage::Push);
                assert_eq!(under_replicated_chunks, Some(vec![1]));
                assert_eq!(suggested, SuggestedAction::Resume);
            }
            other => panic!("expected PendingNeedsAction(Push), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s2_push_retry_succeeds_within_budget() {
        // First-attempt error responses → second attempts succeed.
        // Use 1 chunk, 5 peers, R=5 so we need R distinct successes.
        let bytes = vec![0x11; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.enqueue_send("0xtx-activate");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 200 });
        rpc.enqueue_coverage(coverage_active(1, true));
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        // First-attempt PushAcks: 3 succeed, 2 fail. After retries the
        // 2 failers ACK Ok → R=5 reached.
        for (i, peer_id) in peers.iter().enumerate() {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: if i < 3 { None } else { Some("temporary".into()) },
                },
            })
            .await;
        }
        // Retry success ACKs for the 2 failers.
        for peer_id in &peers[3..5] {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }
        ack_manifest_for_all(&net, merkle_root, &peers).await;

        let pipeline = build_pipeline(
            rpc,
            net.clone(),
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.run(&path).await {
            IngestOutcome::Activated { .. } => (),
            other => panic!("expected Activated after retries, got {other:?}"),
        }
        // Initial 5 + 2 retries = 7 pushes
        assert_eq!(net.push_count(), 7);
    }

    /// Reviewer-required: duplicate `PushAck` from the same peer must
    /// not count twice toward `R`. Setup: 3 distinct peers ACK chunk 0
    /// once each, then peer[0] re-ACKs three more times. With R=5 and
    /// only 3 distinct peers that should leave the chunk under-replicated.
    #[tokio::test]
    async fn s2_duplicate_pushack_from_same_peer_does_not_double_count() {
        let bytes = vec![0x22; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        // Use a 3-archive snapshot but R=5 so we need 5 ACKs (chain
        // would clamp internally; here we drive the under-rep guard).
        let snapshot: Vec<[u8; 20]> = (0..3)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0x10 + i;
                a
            })
            .collect();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..3).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        // 3 distinct peers ACK once each.
        for peer_id in &peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }
        // Now peer[0] re-ACKs 3 more times. If we double-counted, that
        // would push the count to 6; HashSet dedup keeps it at 3.
        for _ in 0..3 {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: peers[0],
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }

        let pipeline = build_pipeline(
            rpc,
            net.clone(),
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.run(&path).await {
            IngestOutcome::PendingNeedsAction {
                failed_stage,
                under_replicated_chunks,
                ..
            } => {
                assert_eq!(failed_stage, IngestStage::Push);
                // 3 distinct peers ≠ R=5; chunk 0 is under-replicated.
                assert_eq!(under_replicated_chunks, Some(vec![0]));
            }
            other => panic!("expected PendingNeedsAction(Push), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s3_manifest_push_one_ack_per_distinct_archive() {
        // Same fixture as happy path but each archive ACKs once; one
        // archive ACKs THREE times (duplicates) and we still need exactly
        // 5 distinct ACKs to proceed. Confirms dedup at the manifest layer.
        let bytes = vec![0x33; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.enqueue_send("0xtx-activate");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 200 });
        rpc.enqueue_coverage(coverage_active(1, true));
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        ack_chunks_for_all(&net, merkle_root, 1, &peers).await;
        // Manifest: 5 distinct archive ACKs, plus 3 DUPES from peer[0].
        for peer_id in &peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: None,
                },
            })
            .await;
        }
        for _ in 0..3 {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: peers[0],
                response: ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: None,
                },
            })
            .await;
        }

        let pipeline = build_pipeline(
            rpc,
            net,
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.run(&path).await {
            IngestOutcome::Activated { .. } => (),
            other => panic!("expected Activated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s4_wallclock_timeout_returns_pending_needs_action_coverage() {
        let bytes = vec![0x44; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());
        // Coverage: never reaches can_activate_now=true. Queue many
        // false responses so the timeout fires before we exhaust.
        for _ in 0..100 {
            rpc.enqueue_coverage(coverage_active(1, false));
        }

        let net = Arc::new(MockNet::new());
        ack_chunks_for_all(&net, merkle_root, 1, &peers).await;
        ack_manifest_for_all(&net, merkle_root, &peers).await;

        // Tight S4 budget so the test runs fast.
        let mut params = params_for_test(defaults_for_tests());
        params.activation_wait_secs = Duration::from_millis(100);

        let pipeline = build_pipeline(rpc.clone(), net, arch_to_peer, my_addr, params);
        match pipeline.run(&path).await {
            IngestOutcome::PendingNeedsAction {
                failed_stage,
                last_coverage,
                suggested,
                ..
            } => {
                assert_eq!(failed_stage, IngestStage::Coverage);
                assert!(last_coverage.is_some());
                assert!(!last_coverage.unwrap().can_activate_now);
                assert_eq!(suggested, SuggestedAction::Resume);
            }
            other => panic!("expected PendingNeedsAction(Coverage), got {other:?}"),
        }
        assert!(rpc.coverage_poll_count() >= 1);
    }

    #[tokio::test]
    async fn s5_activate_failure_returns_pending_needs_action_activate() {
        let bytes = vec![0x55; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        // S5 send fails.
        rpc.enqueue_send_err("ActivateFileV2 rejected: not all chunks attested");
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());
        rpc.enqueue_coverage(coverage_active(1, true));

        let net = Arc::new(MockNet::new());
        ack_chunks_for_all(&net, merkle_root, 1, &peers).await;
        ack_manifest_for_all(&net, merkle_root, &peers).await;

        let pipeline = build_pipeline(
            rpc,
            net,
            arch_to_peer,
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.run(&path).await {
            IngestOutcome::PendingNeedsAction {
                failed_stage,
                last_coverage,
                source,
                suggested,
                ..
            } => {
                assert_eq!(failed_stage, IngestStage::Activate);
                assert!(last_coverage.is_some());
                let s = source.expect("source set on activate failure");
                assert!(s.to_string().contains("not all chunks attested"));
                assert_eq!(suggested, SuggestedAction::Resume);
            }
            other => panic!("expected PendingNeedsAction(Activate), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ingest_params_from_chain_params_round_trips() {
        let cp = ChainParamsInfo {
            chain_id: 31337,
            block_time_ms: 2000,
            finality_depth: 3,
            min_fee: 1_000,
            assignment_replication_factor: 3,
            max_chunk_indices_per_tx: 65_536,
            max_chunk_count_per_file: 1_048_576,
            activation_grace_blocks: 50,
            max_assigned_count_chunk_count: 16_384,
        };
        let p = IngestParams::from_chain_params(&cp, IngestParamsDefaults::default());
        assert_eq!(p.chain_id, 31337);
        assert_eq!(p.assignment_replication_factor, 3);
        assert_eq!(p.max_chunk_indices_per_tx, 65_536);
        assert_eq!(p.max_chunk_count_per_file, 1_048_576);
        assert_eq!(p.activation_grace_blocks, 50);
        assert_eq!(p.fee_per_tx, 1_000);
        // Defaults for the wall-clock knobs.
        assert_eq!(p.poll_interval, Duration::from_secs(2));
    }

    /// Reviewer-required: `PushAck`s from peers NOT assigned to a chunk
    /// must NOT count toward `R` — only the planned target set
    /// `(chunk_index, peer_id)` membership counts. Setup: R=3 with a
    /// 5-archive snapshot. For chunk 0, the V2 assignment picks 3
    /// archives; the other 2 are not assigned to chunk 0. Have 1
    /// assigned peer ACK + the 2 not-assigned peers ACK the same
    /// chunk. The chunk must remain under-replicated (1 valid ACK,
    /// not R=3).
    #[tokio::test]
    async fn s2_unassigned_peer_pushack_does_not_count_toward_replication() {
        let bytes = vec![0x42; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        // Compute the actual V2 assignment for chunk 0 with R=3 — these
        // are the ONLY peers whose ACK should count.
        let assigned = sum_store::assignment_v2::assigned_archives(
            &merkle_root, &snapshot, 0, 3,
        );
        assert_eq!(assigned.len(), 3, "R=3 against 5-archive snapshot");
        let unassigned: Vec<[u8; 20]> = snapshot
            .iter()
            .filter(|a| !assigned.contains(a))
            .copied()
            .collect();
        assert_eq!(unassigned.len(), 2);

        let assigned_peers: Vec<PeerId> = assigned.iter().map(|a| arch_to_peer[a]).collect();
        let unassigned_peers: Vec<PeerId> = unassigned.iter().map(|a| arch_to_peer[a]).collect();

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        // 1 ACK from an assigned peer + 2 ACKs from unassigned peers.
        // If the gate is wrong, this looks like 3 ACKs → activation
        // would proceed. With the gate, only 1 ACK counts → under-replicated.
        net.push_event(SumNetEvent::ShardReceivedV2 {
            peer_id: assigned_peers[0],
            response: ShardResponseV2::PushAck {
                merkle_root,
                chunk_index: 0,
                error: None,
            },
        })
        .await;
        for unassigned_peer in &unassigned_peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *unassigned_peer,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }

        // Use R=3 in params (not R=5 as in the default helper).
        let mut params = params_for_test(defaults_for_tests());
        params.assignment_replication_factor = 3;

        let pipeline = build_pipeline(rpc, net, arch_to_peer, my_addr, params);
        match pipeline.run(&path).await {
            IngestOutcome::PendingNeedsAction {
                failed_stage,
                under_replicated_chunks,
                ..
            } => {
                assert_eq!(failed_stage, IngestStage::Push);
                assert_eq!(
                    under_replicated_chunks,
                    Some(vec![0]),
                    "only assigned-peer ACKs may count toward R"
                );
            }
            other => panic!(
                "unassigned peer ACKs must not falsely satisfy R; got {other:?}"
            ),
        }
    }

    /// Companion regression: S3 sends manifests to the distinct
    /// **assigned** archive set, NOT the whole snapshot. Setup: R=3
    /// against 5 archives. Only the 3 assigned-to-some-chunk archives
    /// need to ACK; the 2 never-assigned archives must NOT block
    /// progression.
    #[tokio::test]
    async fn s3_manifest_targets_only_distinct_assigned_archives() {
        // 1-chunk file so the assignment for that chunk is the entire
        // distinct-assigned set.
        let bytes = vec![0x99; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let assigned = sum_store::assignment_v2::assigned_archives(
            &merkle_root, &snapshot, 0, 3,
        );
        assert_eq!(assigned.len(), 3);
        let assigned_peers: Vec<PeerId> = assigned.iter().map(|a| arch_to_peer[a]).collect();

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.enqueue_send("0xtx-activate");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 200 });
        rpc.enqueue_coverage(coverage_active(1, true));
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        // S2: PushAck from each assigned peer.
        for peer_id in &assigned_peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index: 0,
                    error: None,
                },
            })
            .await;
        }
        // S3: ManifestPushAck from ONLY the 3 assigned peers (no ACK
        // from the 2 unassigned). If S3 were waiting on the whole
        // snapshot, it would time out and fail.
        for peer_id in &assigned_peers {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: *peer_id,
                response: ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: None,
                },
            })
            .await;
        }

        let mut params = params_for_test(defaults_for_tests());
        params.assignment_replication_factor = 3;

        let pipeline = build_pipeline(rpc, net.clone(), arch_to_peer, my_addr, params);
        match pipeline.run(&path).await {
            IngestOutcome::Activated { .. } => (),
            other => panic!("expected Activated with 3-of-5 manifest ACKs; got {other:?}"),
        }
        // S3 sent exactly 3 manifests (assigned set), not 5 (snapshot).
        assert_eq!(
            net.manifest_push_count(),
            3,
            "S3 must target only the distinct-assigned archive set"
        );
    }

    /// W10b regression: `MapPeerResolver` walks the live
    /// `peer_addresses` map and returns the right `PeerId`, or `None`
    /// when the address is unknown. Phase 0b happy-path resolution.
    #[tokio::test]
    async fn map_peer_resolver_finds_peer_by_address() {
        let peer_a = fake_peer();
        let peer_b = fake_peer();
        let addr_a = [0xAA; 20];
        let addr_b = [0xBB; 20];

        let map: Arc<tokio::sync::RwLock<HashMap<PeerId, [u8; 20]>>> = Arc::new(
            tokio::sync::RwLock::new(HashMap::new()),
        );
        {
            let mut w = map.write().await;
            w.insert(peer_a, addr_a);
            w.insert(peer_b, addr_b);
        }

        let resolver = MapPeerResolver::new(map);
        assert_eq!(resolver.resolve(&addr_a), Some(peer_a));
        assert_eq!(resolver.resolve(&addr_b), Some(peer_b));
        // Unknown address → None (does not panic, does not wrap-around).
        assert_eq!(resolver.resolve(&[0xCC; 20]), None);
    }

    /// Reviewer-required regression guard: changing
    /// `activation_grace_blocks` does NOT influence S4 termination.
    /// Activation succeeds even with `activation_grace_blocks = 0`
    /// (which would have aborted under the rejected v1 plan).
    #[tokio::test]
    async fn activation_grace_blocks_does_not_short_circuit_s4() {
        let bytes = vec![0x66; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);

        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.enqueue_send("0xtx-register");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 100 });
        rpc.enqueue_send("0xtx-activate");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 200 });
        rpc.enqueue_coverage(coverage_active(1, true));
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            pending_file_info(&merkle_root, 1, 50),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        let net = Arc::new(MockNet::new());
        ack_chunks_for_all(&net, merkle_root, 1, &peers).await;
        ack_manifest_for_all(&net, merkle_root, &peers).await;

        let mut params = params_for_test(defaults_for_tests());
        params.activation_grace_blocks = 0; // would short-circuit under the bad plan
        params.activation_wait_secs = Duration::from_secs(2); // ample wall-clock

        let pipeline = build_pipeline(rpc, net, arch_to_peer, my_addr, params);
        match pipeline.run(&path).await {
            IngestOutcome::Activated { .. } => (),
            other => panic!("activation_grace_blocks=0 must NOT block S4; got {other:?}"),
        }
    }

    // ── W11 resume tests ────────────────────────────────────────────

    /// Helper: file_info with explicit lifecycle for resume tests.
    fn file_info_with_lifecycle(
        root: &[u8; 32],
        chunk_count: u32,
        assignment_height: u64,
        lifecycle: LifecycleV2,
        activated_at: Option<u64>,
        abandoned_at: Option<u64>,
    ) -> StorageFileInfoV2 {
        StorageFileInfoV2 {
            merkle_root: format!("0x{}", hex::encode(root)),
            owner: l1_address_base58(&[0x01; 20]),
            plaintext_size_bytes: 1024,
            stored_size_bytes: 1024,
            chunk_count,
            fee_pool: 1000,
            created_at: 100,
            activated_at_height: activated_at,
            abandoned_at_height: abandoned_at,
            assignment_height,
            visibility: VisibilityV2::PUBLIC,
            lifecycle,
            access_list: vec![],
        }
    }

    #[tokio::test]
    async fn resume_active_file_returns_activated_on_chain() {
        let bytes = vec![0xA1; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                1,
                50,
                LifecycleV2::ACTIVE,
                Some(150),
                None,
            ),
        );
        let net = Arc::new(MockNet::new());

        let pipeline = build_pipeline(
            rpc.clone(),
            net.clone(),
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.resume(merkle_root, &path).await {
            IngestOutcome::ActivatedOnChain {
                register_height,
                activate_height,
                ..
            } => {
                assert_eq!(register_height, 100);
                assert_eq!(activate_height, 150);
            }
            other => panic!("expected ActivatedOnChain, got {other:?}"),
        }
        // No tx submitted, no peer pushed, no manifest sent — pure no-op resume.
        assert_eq!(net.push_count(), 0);
        assert_eq!(net.manifest_push_count(), 0);
    }

    #[tokio::test]
    async fn resume_abandoned_file_returns_abandoned_on_chain() {
        let bytes = vec![0xA2; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                1,
                50,
                LifecycleV2::ABANDONED,
                None,
                Some(175),
            ),
        );
        let net = Arc::new(MockNet::new());

        let pipeline = build_pipeline(
            rpc,
            net.clone(),
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.resume(merkle_root, &path).await {
            IngestOutcome::AbandonedOnChain {
                merkle_root: r,
                abandoned_at_height,
                ..
            } => {
                assert_eq!(r, merkle_root);
                assert_eq!(abandoned_at_height, Some(175));
            }
            other => panic!("expected AbandonedOnChain, got {other:?}"),
        }
        assert_eq!(net.push_count(), 0);
    }

    #[tokio::test]
    async fn resume_pending_can_activate_jumps_to_s5() {
        let bytes = vec![0xA3; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 7);
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                1,
                50,
                LifecycleV2::PENDING,
                None,
                None,
            ),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());
        // Coverage probe says we can already activate.
        rpc.enqueue_coverage(coverage_active(1, true));
        // S5: send + finalize.
        rpc.enqueue_send("0xtx-activate-resume");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 999 });

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc,
            net.clone(),
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.resume(merkle_root, &path).await {
            IngestOutcome::ResumedActivated {
                activate_tx_hash,
                activate_height,
                register_height,
                ..
            } => {
                assert_eq!(activate_tx_hash, "0xtx-activate-resume");
                assert_eq!(activate_height, 999);
                // register_height comes from chain state (created_at);
                // the typed variant intentionally has no field for the
                // original register_tx_hash because chain doesn't expose it.
                assert_eq!(register_height, 100);
            }
            other => panic!(
                "expected ResumedActivated (resume jumped to S5), got {other:?}"
            ),
        }
        // No pushes, no manifest — coverage was already complete.
        assert_eq!(net.push_count(), 0);
        assert_eq!(net.manifest_push_count(), 0);
    }

    #[tokio::test]
    async fn resume_pending_with_missing_chunks_pushes_only_missing() {
        // 5-chunk file, R=3, snapshot of 5. Coverage missing = {1, 3}.
        // Resume must push ONLY those two chunks, then S3 manifest, then S4, then S5.
        let bytes = vec![0xA4; 5 * 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let peers: Vec<PeerId> = (0..5).map(|_| fake_peer()).collect();
        let arch_to_peer: HashMap<_, _> = snapshot.iter().zip(peers.iter()).map(|(a, p)| (*a, *p)).collect();

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                5,
                50,
                LifecycleV2::PENDING,
                None,
                None,
            ),
        );
        rpc.add_snapshot(50, snapshot.iter().map(node_record).collect());

        // Coverage probe (initial): can_activate_now = false.
        let mut cov_initial = coverage_active(5, false);
        cov_initial.missing_indices = vec![1, 3];
        cov_initial.missing_total = 2;
        rpc.enqueue_coverage(cov_initial);
        // collect_missing_indices pages (offset=0) — same shape, then
        // (offset=4) — empty, terminating the loop.
        let mut cov_page1 = coverage_active(5, false);
        cov_page1.missing_indices = vec![1, 3];
        cov_page1.missing_total = 2;
        rpc.enqueue_coverage(cov_page1);
        let mut cov_page2 = coverage_active(5, false);
        cov_page2.missing_indices = vec![];
        cov_page2.missing_total = 0;
        rpc.enqueue_coverage(cov_page2);
        // S4 wait — eventually can_activate_now = true.
        rpc.enqueue_coverage(coverage_active(5, true));
        // S5.
        rpc.enqueue_send("0xtx-activate-resume");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 1234 });

        // Pre-queue PushAcks for ONLY chunks 1 and 3 from all 3 of
        // their assigned archives. Use R=3 in params.
        let mut params = params_for_test(defaults_for_tests());
        params.assignment_replication_factor = 3;
        let r = 3;
        let net = Arc::new(MockNet::new());
        for chunk_idx in [1u32, 3u32] {
            let assigned = sum_store::assignment_v2::assigned_archives(
                &merkle_root, &snapshot, chunk_idx, r,
            );
            for archive in &assigned {
                net.push_event(SumNetEvent::ShardReceivedV2 {
                    peer_id: arch_to_peer[archive],
                    response: ShardResponseV2::PushAck {
                        merkle_root,
                        chunk_index: chunk_idx,
                        error: None,
                    },
                })
                .await;
            }
        }
        // Manifest ACKs from every distinct-assigned archive (full file's set).
        let mut full_assigned: BTreeSet<[u8; 20]> = BTreeSet::new();
        for chunk_idx in 0..5u32 {
            for a in sum_store::assignment_v2::assigned_archives(
                &merkle_root, &snapshot, chunk_idx, r,
            ) {
                full_assigned.insert(a);
            }
        }
        for archive in &full_assigned {
            net.push_event(SumNetEvent::ShardReceivedV2 {
                peer_id: arch_to_peer[archive],
                response: ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: None,
                },
            })
            .await;
        }

        let pipeline = build_pipeline(rpc, net.clone(), arch_to_peer, my_addr, params);
        match pipeline.resume(merkle_root, &path).await {
            IngestOutcome::ResumedActivated { .. } => (),
            other => panic!("expected ResumedActivated after resume; got {other:?}"),
        }
        // S2 push count = 2 chunks × 3 archives = 6 pushes. Only the
        // missing chunks were sent; chunks 0, 2, 4 were skipped.
        assert_eq!(net.push_count(), 6, "S2 must push only missing chunks");
        // Manifest sent to the full distinct-assigned archive set.
        assert_eq!(
            net.manifest_push_count() as usize,
            full_assigned.len(),
            "S3 must re-push manifest to all distinct assigned archives"
        );
    }

    #[tokio::test]
    async fn resume_root_mismatch_returns_typed_error() {
        let bytes = vec![0xA5; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let actual_root = manifest_dryrun.merkle_root;
        // Operator passes a wrong explicit root.
        let claimed_root = [0xFF; 32];
        assert_ne!(claimed_root, actual_root);

        let rpc = Arc::new(MockRpc::default());
        let net = Arc::new(MockNet::new());

        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.resume(claimed_root, &path).await {
            IngestOutcome::RootMismatch { expected, actual, .. } => {
                assert_eq!(expected, claimed_root);
                assert_eq!(actual, actual_root);
            }
            other => panic!("expected RootMismatch, got {other:?}"),
        }
    }

    /// Reviewer-required regression guard: `lifecycle == Active` plus
    /// `activated_at_height == None` is chain-shape corruption (chain
    /// plan §3.2 sets the height in the same execution that flips
    /// lifecycle). Resume must surface this as `Failed`, NOT report a
    /// silent height of 0.
    #[tokio::test]
    async fn resume_active_with_no_activated_height_is_chain_corruption() {
        let bytes = vec![0xCC; 1_048_576];
        let (_dir, path) = write_test_file(&bytes);
        let snapshot = five_archives();
        let my_addr = snapshot[0];

        let (_mmap, manifest_dryrun) = BinaryChunker::chunk_file(&path).unwrap();
        let merkle_root = manifest_dryrun.merkle_root;

        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                1,
                50,
                LifecycleV2::ACTIVE,
                None, // activated_at_height = None — corruption per chain plan §3.2
                None,
            ),
        );
        let net = Arc::new(MockNet::new());

        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.resume(merkle_root, &path).await {
            IngestOutcome::Failed { stage, source, .. } => {
                assert_eq!(stage, IngestStage::Register);
                assert!(
                    source.to_string().contains("chain shape corruption"),
                    "got: {source}"
                );
            }
            other => panic!(
                "lifecycle=Active + activated_at_height=None must surface as Failed; got {other:?}"
            ),
        }
    }

    // ── W11 abandon tests ───────────────────────────────────────────

    /// Wrong lifecycle → NotAdmissible without burning a tx fee.
    #[tokio::test]
    async fn abandon_wrong_lifecycle_returns_not_admissible() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x01; 32];

        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            file_info_with_lifecycle(
                &merkle_root,
                1,
                50,
                LifecycleV2::ACTIVE,
                Some(150),
                None,
            ),
        );

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc.clone(),
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::NotAdmissible { reason, .. } => {
                assert!(reason.contains("lifecycle"), "got: {reason}");
            }
            other => panic!("expected NotAdmissible, got {other:?}"),
        }
        // No tx submitted.
        assert_eq!(rpc.sent_txs.lock().unwrap().len(), 0);
    }

    /// Pre-grace `current_height < created_at + grace + 1` →
    /// NotAdmissible. earliest_admissible_height must reflect strict-`>`
    /// rule: `created_at + grace + 1`.
    #[tokio::test]
    async fn abandon_pre_grace_returns_not_admissible() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x02; 32];

        // current_height = 149 (created_at=100, grace=50 → admissible at 151)
        struct CurrentHeightRpc {
            inner: Arc<MockRpc>,
            current_height: u64,
        }
        // For these grace tests we override chain_get_block_height by
        // constructing a custom adapter; instead, just monkey-patch the
        // MockRpc default which returns 1000. We need height < 151, so
        // create a custom MockRpc impl below... actually easier: set
        // info.created_at=999_999, grace=50 → earliest=1_000_050;
        // MockRpc::chain_get_block_height returns 1000 → 1000 < 1_000_050.
        let _ = CurrentHeightRpc {
            inner: Arc::new(MockRpc::default()),
            current_height: 0,
        };

        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            StorageFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(merkle_root)),
                owner: l1_address_base58(&[0x01; 20]),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 1,
                fee_pool: 0,
                created_at: 999_900,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 50,
                visibility: VisibilityV2::PUBLIC,
                lifecycle: LifecycleV2::PENDING,
                access_list: vec![],
            },
        );

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc.clone(),
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::NotAdmissible {
                current_height,
                earliest_admissible_height,
                reason,
            } => {
                // MockRpc always returns 1000.
                assert_eq!(current_height, 1000);
                // created_at=999_900, grace=50 → earliest = 999_951.
                assert_eq!(earliest_admissible_height, 999_951);
                assert!(reason.contains("activation grace"), "got: {reason}");
            }
            other => panic!("expected NotAdmissible(pre-grace), got {other:?}"),
        }
        assert_eq!(rpc.sent_txs.lock().unwrap().len(), 0);
    }

    /// **Reviewer-required strict-`>` boundary regression:**
    /// `current_height == created_at + activation_grace_blocks` → still
    /// NOT admissible. Earliest admissible is `created_at + grace + 1`.
    #[tokio::test]
    async fn abandon_at_grace_boundary_returns_not_admissible() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x03; 32];

        // MockRpc::chain_get_block_height always returns 1000.
        // Set created_at=950, grace=50 → grace boundary = 1000.
        // Strict-> rule: must be > 1000; 1000 is the boundary, NOT admissible.
        let rpc = Arc::new(MockRpc::default());
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            StorageFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(merkle_root)),
                owner: l1_address_base58(&[0x01; 20]),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 1,
                fee_pool: 0,
                created_at: 950,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 50,
                visibility: VisibilityV2::PUBLIC,
                lifecycle: LifecycleV2::PENDING,
                access_list: vec![],
            },
        );

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc.clone(),
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::NotAdmissible {
                current_height,
                earliest_admissible_height,
                ..
            } => {
                assert_eq!(current_height, 1000);
                // Strict-> per chain plan §3.5: earliest = 950 + 50 + 1 = 1001.
                // Boundary equality (current == created_at + grace) must
                // still reject — earliest_admissible is +1.
                assert_eq!(earliest_admissible_height, 1001);
            }
            other => panic!("strict-> boundary must reject; got {other:?}"),
        }
        assert_eq!(rpc.sent_txs.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn abandon_just_past_grace_finalizes() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x04; 32];

        // MockRpc returns current_height = 1000.
        // created_at=949, grace=50 → earliest = 949 + 50 + 1 = 1000.
        // 1000 >= 1000 → admissible.
        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 12);
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            StorageFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(merkle_root)),
                owner: l1_address_base58(&[0x01; 20]),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 1,
                fee_pool: 0,
                created_at: 949,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 50,
                visibility: VisibilityV2::PUBLIC,
                lifecycle: LifecycleV2::PENDING,
                access_list: vec![],
            },
        );
        rpc.enqueue_send("0xtx-abandon");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 1010 });

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::Abandoned { tx_hash, finalized_at_height } => {
                assert_eq!(tx_hash, "0xtx-abandon");
                assert_eq!(finalized_at_height, 1010);
            }
            other => panic!("expected Abandoned, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abandon_send_failure_returns_failed() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x05; 32];

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            StorageFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(merkle_root)),
                owner: l1_address_base58(&[0x01; 20]),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 1,
                fee_pool: 0,
                created_at: 949,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 50,
                visibility: VisibilityV2::PUBLIC,
                lifecycle: LifecycleV2::PENDING,
                access_list: vec![],
            },
        );
        rpc.enqueue_send_err("transient mempool reject");

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::Failed { source } => {
                assert!(source.to_string().contains("mempool reject"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abandon_wait_finality_failed_returns_failed() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let merkle_root = [0x06; 32];

        let rpc = Arc::new(MockRpc::default());
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);
        rpc.add_file(
            &format!("0x{}", hex::encode(merkle_root)),
            StorageFileInfoV2 {
                merkle_root: format!("0x{}", hex::encode(merkle_root)),
                owner: l1_address_base58(&[0x01; 20]),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 1,
                fee_pool: 0,
                created_at: 949,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 50,
                visibility: VisibilityV2::PUBLIC,
                lifecycle: LifecycleV2::PENDING,
                access_list: vec![],
            },
        );
        rpc.enqueue_send("0xtx-doomed");
        rpc.enqueue_status(TxStatusV2::Failed {
            block_height: Some(1100),
            reason: "AbandonFileV2 validity check failed".into(),
        });

        let net = Arc::new(MockNet::new());
        let pipeline = build_pipeline(
            rpc,
            net,
            HashMap::new(),
            my_addr,
            params_for_test(defaults_for_tests()),
        );
        match pipeline.abandon(merkle_root).await {
            AbandonOutcome::Failed { source } => {
                assert!(source.to_string().contains("AbandonFileV2 failed"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
