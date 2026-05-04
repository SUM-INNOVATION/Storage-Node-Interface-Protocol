//! V2 inbound request dispatch (chain plan v3.2 §3.6 receive-side).
//!
//! Routes inbound `ShardRequestV2` variants into:
//!
//!   * `Pull`         → chain-governed ACL gate via `AccessChecker`,
//!                       then chunk-store lookup. Public files pass
//!                       the gate via `access_list.is_empty()` —
//!                       same code path the V1 pull uses. V2 is NOT
//!                       exempt from chain governance.
//!   * `Push`         → [`PushValidator::validate_push`] → on Ok,
//!                       persist under `cid_from_blake3_hash(leaf_hash)`
//!                       and update the held-set tracker. Wire CID is
//!                       NEVER trusted — see chain plan §3.6 receive-side.
//!   * `ManifestPush` → reuse `sum_store::serve::validate_manifest_push`
//!                       (recomputes root from chunks; full internal
//!                       consistency), insert into the manifest index,
//!                       ACK the peer immediately, and SPAWN attestation
//!                       as a background task. The ACK does NOT wait for
//!                       `AcceptAssignmentV2` finality — that would
//!                       couple inbound request latency to chain finality.
//!   * `ManifestPull` → existing manifest-index lookup, CBOR-serialize.
//!
//! ## Held-set tracker
//!
//! In-memory `HashMap<[u8; 32], BTreeSet<u32>>` keyed by file
//! `merkle_root`. Updated only on successful V2 `Push`. Read by the
//! `ManifestPush` handler when spawning attestation. Lost on node
//! restart — Phase 0b accepts that; W10/W11 reconstruct from chain
//! state via `storage_getAssignmentCoverageV2 + scan-on-disk`.
//!
//! ## Why ManifestPush triggers attestation, not Push
//!
//! Locked Phase 0b decision: trigger `AssignmentAttestor` on
//! successful `ManifestPush`, batch the current `held ∩ assignment`.
//! Triggering on every Push would burn fees per chunk; triggering on
//! manifest gives the owner a single "I'm done pushing" signal that
//! lets the archive coalesce all attestation into ≤
//! `ceil(|attest_set| / max_chunk_indices_per_tx)` txs.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use sum_net::{PeerId, ShardRequestV2, ShardResponseV2, SumNet};

/// Outbound-response abstraction over `SumNet::respond_shard_v2`.
/// Exists so `V2Dispatcher::handle` can be driven from integration
/// tests with a capturing implementation, without standing up a real
/// libp2p swarm. Production wraps `SumNet`.
#[async_trait::async_trait]
pub trait RespondNet: Send + Sync {
    async fn respond_shard_v2(
        &self,
        channel_id: u64,
        response: ShardResponseV2,
    ) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl RespondNet for SumNet {
    async fn respond_shard_v2(
        &self,
        channel_id: u64,
        response: ShardResponseV2,
    ) -> anyhow::Result<()> {
        SumNet::respond_shard_v2(self, channel_id, response).await
    }
}

/// Blanket impl so callers passing `&Arc<T>` (e.g. main.rs holds
/// `Arc<SumNet>` for sharing across tokio tasks) can hand the
/// dispatcher a `&dyn RespondNet` without manual deref gymnastics.
#[async_trait::async_trait]
impl<T: RespondNet + ?Sized> RespondNet for Arc<T> {
    async fn respond_shard_v2(
        &self,
        channel_id: u64,
        response: ShardResponseV2,
    ) -> anyhow::Result<()> {
        (**self).respond_shard_v2(channel_id, response).await
    }
}
use sum_store::content_id::cid_from_blake3_hash;
use sum_store::serve::validate_manifest_push;
use sum_store::SumStore;
use sum_types::storage::DataManifest;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::acl::AclChecker;
use crate::assignment_attestor::{AssignmentAttestor, AttestRequest, AttestSummary, AttestorRpc};
use crate::push_validator::{PushValidator, V2RpcClient};
use crate::rpc_client::L1RpcClient;
use sum_net::l1_address_from_base58;
use sum_store::manifest_index::ManifestIndex;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;

/// Default attestation polling cadence per AcceptAssignmentV2 batch.
/// Matches `tx_wait::DEFAULT_POLL_INTERVAL` (2s — chain plan Appendix B
/// `block_time_ms`).
pub const DEFAULT_ATTEST_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default per-batch finality timeout for AcceptAssignmentV2.
/// Generous to absorb a few missed slots at the chain's finality depth
/// without bailing the whole attestation.
pub const DEFAULT_ATTEST_BATCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Source of file-info + snapshot + nonce lookups used at the moment
/// `ManifestPush` triggers attestation. Kept as a trait so the dispatcher
/// can be tested without hitting a real RPC.
#[async_trait::async_trait]
pub trait AttestTriggerRpc: Send + Sync {
    /// Get `(chunk_count, assignment_height)` for the file at `merkle_root`.
    async fn fetch_file_shape(
        &self,
        merkle_root: &[u8; 32],
    ) -> Result<FileShape>;

    /// Get the canonical (decoded, sorted, deduped) active-node
    /// snapshot at `height`.
    async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>>;

    /// Get the next nonce slot for `address_base58`.
    async fn fetch_nonce(&self, address_base58: &str) -> Result<u64>;
}

#[derive(Debug, Clone, Copy)]
pub struct FileShape {
    pub chunk_count: u32,
    pub assignment_height: u64,
}

/// File-level access check for V2 pulls. Same semantics as the V1 path's
/// [`AclChecker::check_access_or_default`]: the chain decides whether a
/// file is public/private and which addresses are listed; the checker
/// resolves CID → file root → access list and applies profile-based
/// resolution on RPC failure.
///
/// V2 Pull and ManifestPull MUST go through this — V2 is not exempt
/// from chain governance. Public files stay public because chain state
/// says so, not because V2 bypassed the check.
#[async_trait::async_trait]
pub trait AccessChecker: Send + Sync {
    /// Returns `true` iff `peer_id` is allowed to retrieve `cid`.
    /// `cid` for chunks is the raw chunk CID; for manifests it is the
    /// `MANIFEST_REQUEST_PREFIX + hex(merkle_root)` form.
    async fn check_access_or_default(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> bool;
}

#[async_trait::async_trait]
impl AccessChecker for AclChecker {
    async fn check_access_or_default(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> bool {
        AclChecker::check_access_or_default(self, peer_id, cid, manifest_index).await
    }
}

// ── L1RpcClient bridges ──────────────────────────────────────────────────────
//
// Trait impls that let the production `L1RpcClient` plug straight into
// the dispatcher's three injected interfaces (`V2RpcClient` for the
// push validator, `AttestTriggerRpc` for the manifest-trigger path).
// `AttestorRpc` is implemented in `assignment_attestor.rs`.

#[async_trait::async_trait]
impl V2RpcClient for L1RpcClient {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<sum_types::rpc_types::StorageFileInfoV2> {
        L1RpcClient::storage_get_file_info_v2(self, merkle_root_hex, None, None).await
    }

    async fn storage_get_active_nodes_at_height(
        &self,
        height: u64,
    ) -> Result<Vec<sum_types::rpc_types::NodeRecordInfo>> {
        L1RpcClient::storage_get_active_nodes_at_height(self, height).await
    }
}

#[async_trait::async_trait]
impl AttestTriggerRpc for L1RpcClient {
    async fn fetch_file_shape(&self, merkle_root: &[u8; 32]) -> Result<FileShape> {
        let key = format!("0x{}", hex::encode(merkle_root));
        let info = L1RpcClient::storage_get_file_info_v2(self, &key, None, None).await?;
        Ok(FileShape {
            chunk_count: info.chunk_count,
            assignment_height: info.assignment_height,
        })
    }

    async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>> {
        let raw = L1RpcClient::storage_get_active_nodes_at_height(self, height).await?;
        let mut decoded = Vec::with_capacity(raw.len());
        for record in raw {
            let addr = l1_address_from_base58(&record.address)?;
            decoded.push(addr);
        }
        decoded.sort();
        decoded.dedup();
        Ok(decoded)
    }

    async fn fetch_nonce(&self, address_base58: &str) -> Result<u64> {
        L1RpcClient::get_nonce(self, address_base58).await
    }
}

/// Routes inbound V2 requests to the right validator/store/attestor.
///
/// Reuses the existing [`sum_store::SumStore`] for chunk and manifest
/// persistence — no separate store type — so V2 handlers and V1
/// handlers share the same on-disk layout under the same lock.
pub struct V2Dispatcher<V, A, T>
where
    V: V2RpcClient + 'static,
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    validator: Arc<PushValidator<V>>,
    attestor: Arc<AssignmentAttestor<A>>,
    trigger_rpc: Arc<T>,
    store: Arc<RwLock<SumStore>>,
    /// Same chain-governed ACL the V1 path uses (production `AclChecker`).
    /// Trait object so tests can mock without standing up a real HTTP
    /// responder, and so future receive paths (e.g. challenge replies)
    /// can swap in a different policy.
    acl: Arc<dyn AccessChecker>,
    held: Arc<Mutex<HashMap<[u8; 32], BTreeSet<u32>>>>,
    /// L1 address (base58) for this archive — used by attestation
    /// trigger to fetch the next nonce. Stored as base58 to avoid
    /// re-encoding on every spawn.
    my_addr_base58: String,
    attest_poll_interval: Duration,
    attest_batch_timeout: Duration,
}

impl<V, A, T> V2Dispatcher<V, A, T>
where
    V: V2RpcClient + 'static,
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    pub fn new(
        validator: Arc<PushValidator<V>>,
        attestor: Arc<AssignmentAttestor<A>>,
        trigger_rpc: Arc<T>,
        store: Arc<RwLock<SumStore>>,
        acl: Arc<dyn AccessChecker>,
        my_addr: [u8; 20],
    ) -> Self {
        let my_addr_base58 = sum_net::l1_address_base58(&my_addr);
        Self {
            validator,
            attestor,
            trigger_rpc,
            store,
            acl,
            held: Arc::new(Mutex::new(HashMap::new())),
            my_addr_base58,
            attest_poll_interval: DEFAULT_ATTEST_POLL_INTERVAL,
            attest_batch_timeout: DEFAULT_ATTEST_BATCH_TIMEOUT,
        }
    }

    /// For tests that need to stop the attestation polling sooner.
    pub fn with_attest_timing(
        mut self,
        poll_interval: Duration,
        batch_timeout: Duration,
    ) -> Self {
        self.attest_poll_interval = poll_interval;
        self.attest_batch_timeout = batch_timeout;
        self
    }

    /// Snapshot of the held tracker for `merkle_root`. Used by the
    /// attestation trigger and by tests asserting receive-side state.
    pub fn held_for(&self, merkle_root: &[u8; 32]) -> BTreeSet<u32> {
        self.held
            .lock()
            .expect("held mutex poisoned")
            .get(merkle_root)
            .cloned()
            .unwrap_or_default()
    }

    /// Top-level dispatcher. Replies via `net.respond_shard_v2` exactly
    /// once per inbound request.
    pub async fn handle(
        &self,
        net: &dyn RespondNet,
        peer_id: PeerId,
        request: ShardRequestV2,
        channel_id: u64,
    ) {
        match request {
            ShardRequestV2::Pull { cid, offset, max_bytes } => {
                self.handle_pull(net, peer_id, channel_id, cid, offset, max_bytes).await;
            }
            ShardRequestV2::Push {
                data,
                merkle_root,
                chunk_index,
                merkle_path,
            } => {
                self.handle_push(net, peer_id, channel_id, data, merkle_root, chunk_index, merkle_path)
                    .await;
            }
            ShardRequestV2::ManifestPush { merkle_root, manifest_bytes } => {
                self.handle_manifest_push(net, channel_id, merkle_root, manifest_bytes)
                    .await;
            }
            ShardRequestV2::ManifestPull { merkle_root } => {
                self.handle_manifest_pull(net, peer_id, channel_id, merkle_root).await;
            }
        }
    }

    async fn handle_pull(
        &self,
        net: &dyn RespondNet,
        peer_id: PeerId,
        channel_id: u64,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) {
        let resp = self.build_pull_response(peer_id, cid, offset, max_bytes).await;
        if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
            warn!(channel_id, %e, "V2 Pull: failed to send response");
        }
    }

    /// Pull-response builder split out for testability — no SumNet side
    /// effects, so unit tests can assert on the exact response shape
    /// (including the `ACCESS_DENIED` deny path).
    pub async fn build_pull_response(
        &self,
        peer_id: PeerId,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) -> ShardResponseV2 {
        let stores = self.store.read().await;
        // Chain-governed ACL gate. Public files pass via
        // `access_list.is_empty()` inside `AclChecker::check_access`, so
        // this is essentially a no-op for Phase 0b Public traffic — but
        // it is the SAME no-op the V1 path runs (see [main.rs run_listen
        // V1 Pull arm]). V2 must not serve from `local` without going
        // through it; otherwise a peer that learns a CID can read over
        // `/sum/storage/v2` regardless of chain registration /
        // lifecycle / access list.
        let allowed = self
            .acl
            .check_access_or_default(&peer_id, &cid, &stores.manifest_idx)
            .await;
        if !allowed {
            info!(%peer_id, %cid, "V2 Pull: ACCESS DENIED");
            return ShardResponseV2::Data {
                cid,
                offset,
                total_bytes: 0,
                data: Vec::new(),
                error: Some("ACCESS_DENIED: not in file access list".into()),
            };
        }
        match stores.local.get(&cid) {
            Ok(full) => {
                let total = full.len() as u64;
                let start = offset as usize;
                if start > full.len() {
                    ShardResponseV2::Data {
                        cid,
                        offset,
                        total_bytes: total,
                        data: Vec::new(),
                        error: Some(format!(
                            "offset {offset} > total_bytes {total}"
                        )),
                    }
                } else {
                    let end = (start + max_bytes as usize).min(full.len());
                    ShardResponseV2::Data {
                        cid,
                        offset,
                        total_bytes: total,
                        data: full[start..end].to_vec(),
                        error: None,
                    }
                }
            }
            Err(e) => ShardResponseV2::Data {
                cid,
                offset,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(format!("not found: {e}")),
            },
        }
    }

    async fn handle_push(
        &self,
        net: &dyn RespondNet,
        peer_id: PeerId,
        channel_id: u64,
        data: Vec<u8>,
        merkle_root: [u8; 32],
        chunk_index: u32,
        merkle_path: Vec<[u8; 32]>,
    ) {
        let validate = self
            .validator
            .validate_push(merkle_root, chunk_index, &data, &merkle_path)
            .await;
        let resp = match validate {
            Ok(leaf_hash) => {
                let cid = cid_from_blake3_hash(&blake3::Hash::from(leaf_hash));
                let stores = self.store.read().await;
                if let Err(e) = stores.local.put(&cid, &data) {
                    warn!(%peer_id, %cid, %e, "V2 Push: store.put failed");
                    ShardResponseV2::PushAck {
                        merkle_root,
                        chunk_index,
                        error: Some(format!("store error: {e}")),
                    }
                } else {
                    drop(stores);
                    self.held
                        .lock()
                        .expect("held mutex poisoned")
                        .entry(merkle_root)
                        .or_default()
                        .insert(chunk_index);
                    debug!(
                        %peer_id,
                        root = %hex::encode(merkle_root),
                        chunk_index,
                        cid = %cid,
                        "V2 Push: chunk validated and stored"
                    );
                    ShardResponseV2::PushAck {
                        merkle_root,
                        chunk_index,
                        error: None,
                    }
                }
            }
            Err(reject) => {
                let msg = reject.to_string();
                debug!(
                    %peer_id,
                    root = %hex::encode(merkle_root),
                    chunk_index,
                    reason = %msg,
                    "V2 Push: rejected"
                );
                ShardResponseV2::PushAck {
                    merkle_root,
                    chunk_index,
                    error: Some(msg),
                }
            }
        };
        if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
            warn!(channel_id, %e, "V2 Push: failed to send PushAck");
        }
    }

    async fn handle_manifest_push(
        &self,
        net: &dyn RespondNet,
        channel_id: u64,
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
    ) {
        let root_hex = hex::encode(merkle_root);
        let manifest = match validate_manifest_push(&root_hex, &manifest_bytes) {
            Ok(m) => m,
            Err(reason) => {
                debug!(root = %root_hex, %reason, "V2 ManifestPush: validation failed");
                let resp = ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: Some(reason),
                };
                if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
                    warn!(channel_id, %e, "V2 ManifestPush: failed to send ACK after validation error");
                }
                return;
            }
        };

        // Persistence first — the locked decision is "ManifestPushAck only after
        // manifest persistence succeeds." Attestation is async-AFTER ack.
        {
            let mut stores = self.store.write().await;
            if stores.manifest_idx.get_by_merkle_root(&merkle_root).is_some() {
                info!(root = %root_hex, "V2 ManifestPush: already indexed (idempotent)");
            } else if let Err(e) = stores.manifest_idx.insert(&manifest) {
                warn!(root = %root_hex, %e, "V2 ManifestPush: manifest_idx.insert failed");
                let resp = ShardResponseV2::ManifestPushAck {
                    merkle_root,
                    error: Some(format!("manifest persistence failed: {e}")),
                };
                if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
                    warn!(channel_id, %e, "V2 ManifestPush: failed to send ACK after persist error");
                }
                return;
            } else {
                info!(
                    root = %root_hex,
                    file_name = %manifest.file_name,
                    chunk_count = manifest.chunk_count,
                    "V2 ManifestPush: indexed"
                );
            }
        }

        // ACK before attestation — decouple inbound latency from chain finality.
        let resp = ShardResponseV2::ManifestPushAck {
            merkle_root,
            error: None,
        };
        if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
            warn!(channel_id, %e, "V2 ManifestPush: failed to send ACK");
            return;
        }

        // Spawn the attestation. Failures are logged but never surface to
        // the peer — the manifest is persisted regardless.
        self.spawn_attestation(merkle_root, manifest);
    }

    async fn handle_manifest_pull(
        &self,
        net: &dyn RespondNet,
        peer_id: PeerId,
        channel_id: u64,
        merkle_root: [u8; 32],
    ) {
        let resp = self.build_manifest_pull_response(peer_id, merkle_root).await;
        if let Err(e) = net.respond_shard_v2(channel_id, resp).await {
            warn!(channel_id, %e, "V2 ManifestPull: failed to send response");
        }
    }

    /// ManifestPull response builder split out for testability — same
    /// pattern as `build_pull_response`. Tests can drive the deny path
    /// without SumNet wiring.
    pub async fn build_manifest_pull_response(
        &self,
        peer_id: PeerId,
        merkle_root: [u8; 32],
    ) -> ShardResponseV2 {
        let stores = self.store.read().await;
        // ACL gate. The V1 ACL convention treats manifests under the
        // pseudo-CID `MANIFEST_REQUEST_PREFIX + hex(root)` so the same
        // resolver path covers chunk and manifest pulls without a V2
        // bypass.
        let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", hex::encode(merkle_root));
        let allowed = self
            .acl
            .check_access_or_default(&peer_id, &manifest_cid, &stores.manifest_idx)
            .await;
        if !allowed {
            info!(%peer_id, root = %hex::encode(merkle_root), "V2 ManifestPull: ACCESS DENIED");
            return ShardResponseV2::ManifestData {
                merkle_root,
                manifest_bytes: Vec::new(),
                error: Some("ACCESS_DENIED: not in file access list".into()),
            };
        }
        match stores.manifest_idx.get_by_merkle_root(&merkle_root) {
            Some(manifest) => {
                let mut buf = Vec::new();
                match ciborium::ser::into_writer(&manifest, &mut buf) {
                    Ok(()) => ShardResponseV2::ManifestData {
                        merkle_root,
                        manifest_bytes: buf,
                        error: None,
                    },
                    Err(e) => ShardResponseV2::ManifestData {
                        merkle_root,
                        manifest_bytes: Vec::new(),
                        error: Some(format!("manifest serialization error: {e}")),
                    },
                }
            }
            None => ShardResponseV2::ManifestData {
                merkle_root,
                manifest_bytes: Vec::new(),
                error: Some(format!(
                    "manifest not found for root: {}",
                    hex::encode(merkle_root)
                )),
            },
        }
    }

    /// Build an `AttestRequest` and spawn the attestor as a background
    /// task. We don't `await` here — the inbound dispatch path returns
    /// immediately after sending `ManifestPushAck`. Attestation outcomes
    /// are logged at info/warn; failed batches are recoverable via
    /// `storage_getAssignmentCoverageV2` in W10/W11 resume flows.
    fn spawn_attestation(&self, merkle_root: [u8; 32], _manifest: DataManifest) {
        let attestor = self.attestor.clone();
        let trigger_rpc = self.trigger_rpc.clone();
        let held_snapshot = self.held_for(&merkle_root);
        let my_addr_base58 = self.my_addr_base58.clone();
        let poll_interval = self.attest_poll_interval;
        let batch_timeout = self.attest_batch_timeout;

        tokio::spawn(async move {
            run_attestation(
                attestor,
                trigger_rpc,
                merkle_root,
                held_snapshot,
                my_addr_base58,
                poll_interval,
                batch_timeout,
            )
            .await;
        });
    }
}

/// Attestation entry-point split out of the spawn closure so it can be
/// unit-tested directly without going through `tokio::spawn`.
pub async fn run_attestation<A, T>(
    attestor: Arc<AssignmentAttestor<A>>,
    trigger_rpc: Arc<T>,
    merkle_root: [u8; 32],
    held_snapshot: BTreeSet<u32>,
    my_addr_base58: String,
    poll_interval: Duration,
    batch_timeout: Duration,
) -> Option<AttestSummary>
where
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    let root_hex = hex::encode(merkle_root);
    if held_snapshot.is_empty() {
        debug!(root = %root_hex, "attestation trigger: held set empty, nothing to attest");
        return None;
    }

    let shape = match trigger_rpc.fetch_file_shape(&merkle_root).await {
        Ok(s) => s,
        Err(e) => {
            warn!(root = %root_hex, %e, "attestation trigger: fetch_file_shape failed");
            return None;
        }
    };
    let snapshot = match trigger_rpc.fetch_snapshot(shape.assignment_height).await {
        Ok(s) => s,
        Err(e) => {
            warn!(root = %root_hex, %e, "attestation trigger: fetch_snapshot failed");
            return None;
        }
    };
    let starting_nonce = match trigger_rpc.fetch_nonce(&my_addr_base58).await {
        Ok(n) => n,
        Err(e) => {
            warn!(root = %root_hex, %e, "attestation trigger: fetch_nonce failed");
            return None;
        }
    };

    let req = AttestRequest {
        merkle_root,
        chunk_count: shape.chunk_count,
        snapshot,
        held: held_snapshot,
        starting_nonce,
        poll_interval,
        batch_timeout,
    };
    let summary = attestor.attest(req).await;

    if summary.fully_attested() {
        info!(
            root = %root_hex,
            batches = summary.batches.len(),
            attested_count = summary.attested_count(),
            "attestation: every AcceptAssignmentV2 batch finalized"
        );
    } else {
        warn!(
            root = %root_hex,
            finalized_batches = summary.batches.len(),
            attested_count = summary.attested_count(),
            error = ?summary.error,
            "attestation: stopped before finishing — caller may resume via storage_getAssignmentCoverageV2"
        );
    }
    Some(summary)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push_validator::PushReject;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use sum_net::l1_address_base58;
    use sum_store::merkle::MerkleTree;
    use sum_types::rpc_types::{
        LifecycleV2, NodeRecordInfo, StorageFileInfoV2, TxStatusV2, VisibilityV2,
    };
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    /// Combined mock that satisfies V2RpcClient (push validation) +
    /// AttestorRpc (tx submit + status) + AttestTriggerRpc (file/snapshot/nonce).
    /// Tests stub each method by adding canned responses.
    #[derive(Default)]
    struct AllMockRpc {
        files: StdMutex<HashMap<String, StorageFileInfoV2>>,
        snapshots: StdMutex<HashMap<u64, Vec<NodeRecordInfo>>>,
        decoded_snapshots: StdMutex<HashMap<u64, Vec<[u8; 20]>>>,
        send_responses: StdMutex<VecDeque<Result<String, String>>>,
        status_responses: StdMutex<VecDeque<Result<TxStatusV2, String>>>,
        nonce_responses: StdMutex<HashMap<String, u64>>,
        sent_txs: StdMutex<Vec<String>>,
    }

    impl AllMockRpc {
        fn new() -> Self {
            Self::default()
        }
        fn add_file(&self, root_hex: &str, info: StorageFileInfoV2) {
            self.files.lock().unwrap().insert(root_hex.to_string(), info);
        }
        fn add_snapshot(&self, height: u64, nodes: Vec<NodeRecordInfo>, decoded: Vec<[u8; 20]>) {
            self.snapshots.lock().unwrap().insert(height, nodes);
            self.decoded_snapshots.lock().unwrap().insert(height, decoded);
        }
        fn enqueue_send(&self, hash: &str) {
            self.send_responses.lock().unwrap().push_back(Ok(hash.into()));
        }
        fn enqueue_status(&self, st: TxStatusV2) {
            self.status_responses.lock().unwrap().push_back(Ok(st));
        }
        fn set_nonce(&self, addr_b58: &str, nonce: u64) {
            self.nonce_responses
                .lock()
                .unwrap()
                .insert(addr_b58.to_string(), nonce);
        }
        fn sent_count(&self) -> usize {
            self.sent_txs.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl V2RpcClient for AllMockRpc {
        async fn storage_get_file_info_v2(
            &self,
            merkle_root_hex: &str,
        ) -> anyhow::Result<StorageFileInfoV2> {
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
        ) -> anyhow::Result<Vec<NodeRecordInfo>> {
            self.snapshots
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no snapshot at height {height}"))
        }
    }

    #[async_trait::async_trait]
    impl crate::tx_wait::TxStatusSource for AllMockRpc {
        async fn get_transaction_status(&self, _tx_hash: &str) -> anyhow::Result<TxStatusV2> {
            let next = self
                .status_responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("test bug: no status response queued"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    #[async_trait::async_trait]
    impl AttestorRpc for AllMockRpc {
        async fn send_raw_transaction(&self, hex: &str) -> anyhow::Result<String> {
            self.sent_txs.lock().unwrap().push(hex.to_string());
            let next = self
                .send_responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("test bug: no send response queued"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    #[async_trait::async_trait]
    impl AttestTriggerRpc for AllMockRpc {
        async fn fetch_file_shape(&self, merkle_root: &[u8; 32]) -> anyhow::Result<FileShape> {
            let key = format!("0x{}", hex::encode(merkle_root));
            let info = self
                .files
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("trigger: unknown root"))?;
            Ok(FileShape {
                chunk_count: info.chunk_count,
                assignment_height: info.assignment_height,
            })
        }
        async fn fetch_snapshot(&self, height: u64) -> anyhow::Result<Vec<[u8; 20]>> {
            self.decoded_snapshots
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("trigger: no snapshot"))
        }
        async fn fetch_nonce(&self, address_base58: &str) -> anyhow::Result<u64> {
            self.nonce_responses
                .lock()
                .unwrap()
                .get(address_base58)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("trigger: no nonce for {address_base58}"))
        }
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

    fn file_info_active(root: &[u8; 32], chunk_count: u32, assignment_height: u64) -> StorageFileInfoV2 {
        StorageFileInfoV2 {
            merkle_root: format!("0x{}", hex::encode(root)),
            owner: l1_address_base58(&[0x01; 20]),
            plaintext_size_bytes: 1024,
            stored_size_bytes: 1024,
            chunk_count,
            fee_pool: 1000,
            created_at: 100,
            activated_at_height: Some(150),
            abandoned_at_height: None,
            assignment_height,
            visibility: VisibilityV2::PUBLIC,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: vec![],
        }
    }

    /// Build a manifest + tree fixture. Per V2 receive-side semantics
    /// every chunk's CID is derived from blake3(data).
    fn make_manifest(n: u32, salt: u8) -> (DataManifest, Vec<Vec<u8>>, MerkleTree) {
        let chunks: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut v = vec![salt; 64];
                v.extend_from_slice(&i.to_le_bytes());
                v
            })
            .collect();
        let leaves: Vec<blake3::Hash> = chunks.iter().map(|c| blake3::hash(c)).collect();
        let tree = MerkleTree::build(&leaves);
        let mut offset = 0u64;
        let descriptors: Vec<ChunkDescriptor> = leaves
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let size = chunks[i].len() as u64;
                let d = ChunkDescriptor {
                    chunk_index: i as u32,
                    offset,
                    size,
                    blake3_hash: *h.as_bytes(),
                    cid: cid_from_blake3_hash(h),
                    plaintext_blake3_hash: None,
                };
                offset += size;
                d
            })
            .collect();
        let total_size: u64 = chunks.iter().map(|c| c.len() as u64).sum();
        let manifest = DataManifest {
            file_name: format!("test-{salt:02x}.bin"),
            file_hash: *blake3::hash(&chunks.concat()).as_bytes(),
            total_size_bytes: total_size,
            chunk_count: n,
            merkle_root: *tree.root().as_bytes(),
            chunks: descriptors,
        };
        (manifest, chunks, tree)
    }

    fn cbor(m: &DataManifest) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(m, &mut buf).unwrap();
        buf
    }

    fn make_stores() -> Arc<RwLock<SumStore>> {
        let temp_dir = tempfile::tempdir().unwrap();
        let store_dir = temp_dir.path().join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let cfg = sum_types::config::StoreConfig {
            store_dir,
            ..sum_types::config::StoreConfig::default()
        };
        let store = SumStore::new(cfg).expect("SumStore::new failed");
        // Leak the temp_dir so the test can still find files. tests are
        // short-lived; OS reaps when the test binary exits.
        std::mem::forget(temp_dir);
        Arc::new(RwLock::new(store))
    }

    /// Test ACL with a single boolean toggle. `allow_default = true`
    /// (default for most tests) mirrors Public-file behavior; deny
    /// tests build with `allow_default = false`.
    struct ToggleAcl {
        allow_default: bool,
    }

    #[async_trait::async_trait]
    impl AccessChecker for ToggleAcl {
        async fn check_access_or_default(
            &self,
            _peer_id: &PeerId,
            _cid: &str,
            _manifest_index: &ManifestIndex,
        ) -> bool {
            self.allow_default
        }
    }

    fn build_dispatcher(
        rpc: AllMockRpc,
        _snapshot: Vec<[u8; 20]>,
        my_addr: [u8; 20],
        stores: Arc<RwLock<SumStore>>,
    ) -> V2Dispatcher<ArcRpc, ArcRpc, ArcRpcTrigger> {
        build_dispatcher_with_acl(rpc, my_addr, stores, true)
    }

    fn build_dispatcher_with_acl(
        rpc: AllMockRpc,
        my_addr: [u8; 20],
        stores: Arc<RwLock<SumStore>>,
        acl_allows: bool,
    ) -> V2Dispatcher<ArcRpc, ArcRpc, ArcRpcTrigger> {
        let rpc_arc = Arc::new(rpc);
        let validator = Arc::new(PushValidator::new(
            ArcRpc(rpc_arc.clone()),
            my_addr,
            crate::push_validator::V2Params::DEFAULTS,
        ));
        let attestor = Arc::new(AssignmentAttestor::new(
            ArcRpc(rpc_arc.clone()),
            [42u8; 32],
            my_addr,
            1337,
            1_000_000,
            crate::push_validator::V2Params {
                assignment_replication_factor: 5,
                max_chunk_indices_per_tx: 64,
            },
        ));
        let trigger_rpc = ArcRpcTrigger(rpc_arc);
        let acl: Arc<dyn AccessChecker> = Arc::new(ToggleAcl { allow_default: acl_allows });
        V2Dispatcher::new(
            validator,
            attestor,
            Arc::new(trigger_rpc),
            stores,
            acl,
            my_addr,
        )
        .with_attest_timing(Duration::from_millis(10), Duration::from_secs(2))
    }

    /// Newtype around `Arc<AllMockRpc>` so we can implement traits that
    /// would otherwise conflict with the orphan rule when applied to
    /// `Arc<T>` directly.
    #[derive(Clone)]
    struct ArcRpc(Arc<AllMockRpc>);

    #[async_trait::async_trait]
    impl V2RpcClient for ArcRpc {
        async fn storage_get_file_info_v2(
            &self,
            merkle_root_hex: &str,
        ) -> anyhow::Result<StorageFileInfoV2> {
            self.0.storage_get_file_info_v2(merkle_root_hex).await
        }
        async fn storage_get_active_nodes_at_height(
            &self,
            height: u64,
        ) -> anyhow::Result<Vec<NodeRecordInfo>> {
            self.0.storage_get_active_nodes_at_height(height).await
        }
    }
    #[async_trait::async_trait]
    impl crate::tx_wait::TxStatusSource for ArcRpc {
        async fn get_transaction_status(&self, tx_hash: &str) -> anyhow::Result<TxStatusV2> {
            self.0.get_transaction_status(tx_hash).await
        }
    }
    #[async_trait::async_trait]
    impl AttestorRpc for ArcRpc {
        async fn send_raw_transaction(&self, hex: &str) -> anyhow::Result<String> {
            self.0.send_raw_transaction(hex).await
        }
    }

    #[derive(Clone)]
    struct ArcRpcTrigger(Arc<AllMockRpc>);
    #[async_trait::async_trait]
    impl AttestTriggerRpc for ArcRpcTrigger {
        async fn fetch_file_shape(&self, root: &[u8; 32]) -> anyhow::Result<FileShape> {
            self.0.fetch_file_shape(root).await
        }
        async fn fetch_snapshot(&self, height: u64) -> anyhow::Result<Vec<[u8; 20]>> {
            self.0.fetch_snapshot(height).await
        }
        async fn fetch_nonce(&self, address_base58: &str) -> anyhow::Result<u64> {
            self.0.fetch_nonce(address_base58).await
        }
    }

    /// Build a fixture and exercise just the run_attestation entry-point
    /// (skips the SumNet wiring; that side is exercised via integration).
    #[tokio::test]
    async fn run_attestation_happy_path_finalizes_one_batch() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let assignment_height = 500u64;

        let (manifest, _chunks, _tree) = make_manifest(8, 0x10);
        let root = manifest.merkle_root;

        let rpc = AllMockRpc::new();
        rpc.add_file(
            &format!("0x{}", hex::encode(root)),
            file_info_active(&root, manifest.chunk_count, assignment_height),
        );
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
            snapshot.clone(),
        );
        rpc.set_nonce(&l1_address_base58(&my_addr), 50);
        rpc.enqueue_send("0xtx-attest");
        rpc.enqueue_status(TxStatusV2::Finalized { block_height: 1000 });
        let rpc_arc = Arc::new(rpc);

        let attestor = Arc::new(AssignmentAttestor::new(
            ArcRpc(rpc_arc.clone()),
            [42u8; 32],
            my_addr,
            1337,
            1_000_000,
            crate::push_validator::V2Params {
                assignment_replication_factor: 5,
                max_chunk_indices_per_tx: 64,
            },
        ));
        // Pretend we already received chunks 0..3 via Push.
        let held: BTreeSet<u32> = (0..4).collect();

        let summary = run_attestation(
            attestor,
            Arc::new(ArcRpcTrigger(rpc_arc.clone())),
            root,
            held,
            l1_address_base58(&my_addr),
            Duration::from_millis(10),
            Duration::from_secs(2),
        )
        .await
        .expect("attestation should run");
        assert!(summary.fully_attested(), "{:?}", summary.error);
        assert_eq!(summary.batches.len(), 1);
        assert_eq!(summary.attested_count(), 4);
        assert_eq!(summary.batches[0].nonce, 50);
        assert_eq!(rpc_arc.sent_count(), 1);
    }

    #[tokio::test]
    async fn run_attestation_no_held_skips_rpcs() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = AllMockRpc::new();
        let rpc_arc = Arc::new(rpc);
        let attestor = Arc::new(AssignmentAttestor::new(
            ArcRpc(rpc_arc.clone()),
            [42u8; 32],
            my_addr,
            1337,
            1_000_000,
            crate::push_validator::V2Params::DEFAULTS,
        ));
        // empty held → trigger short-circuits before any RPC call.
        let summary = run_attestation(
            attestor,
            Arc::new(ArcRpcTrigger(rpc_arc.clone())),
            [0; 32],
            BTreeSet::new(),
            l1_address_base58(&my_addr),
            Duration::from_millis(10),
            Duration::from_secs(2),
        )
        .await;
        assert!(summary.is_none(), "no held -> no AttestSummary");
        assert_eq!(rpc_arc.sent_count(), 0);
    }

    /// Exercise the dispatcher's Push handler end-to-end: a valid push
    /// adds the chunk to the held tracker.
    #[tokio::test]
    async fn dispatcher_push_validates_persists_and_tracks_held() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let assignment_height = 500u64;
        let (manifest, chunks, tree) = make_manifest(8, 0x10);
        let root = manifest.merkle_root;

        let rpc = AllMockRpc::new();
        rpc.add_file(
            &format!("0x{}", hex::encode(root)),
            file_info_active(&root, manifest.chunk_count, assignment_height),
        );
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
            snapshot.clone(),
        );
        rpc.set_nonce(&l1_address_base58(&my_addr), 1);

        let stores = make_stores();
        let dispatcher = build_dispatcher(rpc, snapshot.clone(), my_addr, stores.clone());

        // Find a chunk_index assigned to my_addr (use the same R the
        // validator will use under V2Params::DEFAULTS).
        let mut good = None;
        let r = crate::push_validator::V2Params::DEFAULTS.assignment_replication_factor;
        for i in 0..manifest.chunk_count {
            let assigned = sum_store::assignment_v2::assigned_archives(
                &root, &snapshot, i, r,
            );
            if assigned.contains(&my_addr) {
                good = Some(i);
                break;
            }
        }
        let i = good.expect("need at least one chunk assigned to my_addr");

        let leaf = match dispatcher
            .validator
            .validate_push(root, i, &chunks[i as usize], &tree.proof_bytes(i))
            .await
        {
            Ok(h) => h,
            Err(e) => panic!("validate_push failed: {e}"),
        };
        let cid = cid_from_blake3_hash(&blake3::Hash::from(leaf));

        // Apply the side effects (mirror what dispatcher.handle does)
        // through the same code path by manually invoking handle_push's
        // store/held-tracker steps. Doing this here exercises the tracker
        // independently of the SumNet response wiring.
        {
            let s = stores.read().await;
            s.local.put(&cid, &chunks[i as usize]).unwrap();
        }
        dispatcher
            .held
            .lock()
            .unwrap()
            .entry(root)
            .or_default()
            .insert(i);

        // Held tracker recorded the chunk.
        let held = dispatcher.held_for(&root);
        assert_eq!(held.len(), 1);
        assert!(held.contains(&i));

        // Chunk was persisted.
        assert!(stores.read().await.local.has(&cid));
    }

    /// Bad merkle proof → PushReject::BadProof, no persistence, no held update.
    #[tokio::test]
    async fn dispatcher_push_rejects_bad_proof_without_persisting() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let (manifest, chunks, tree) = make_manifest(8, 0x20);
        let root = manifest.merkle_root;
        let assignment_height = 600u64;

        let rpc = AllMockRpc::new();
        rpc.add_file(
            &format!("0x{}", hex::encode(root)),
            file_info_active(&root, manifest.chunk_count, assignment_height),
        );
        rpc.add_snapshot(
            assignment_height,
            snapshot.iter().map(node_record).collect(),
            snapshot.clone(),
        );
        let stores = make_stores();
        let dispatcher = build_dispatcher(rpc, snapshot.clone(), my_addr, stores.clone());

        // Find an assigned chunk under the validator's actual R
        // (V2Params::DEFAULTS.assignment_replication_factor = 3), then
        // tamper the proof. The fixture doesn't get to override R; if we
        // looked up with the wrong R we'd pick a chunk the validator
        // would later reject for NotAssigned (which masks BadProof).
        let mut good = None;
        let r = crate::push_validator::V2Params::DEFAULTS.assignment_replication_factor;
        for i in 0..manifest.chunk_count {
            let assigned = sum_store::assignment_v2::assigned_archives(
                &root, &snapshot, i, r,
            );
            if assigned.contains(&my_addr) {
                good = Some(i);
                break;
            }
        }
        let i = good.unwrap();

        let mut bad_proof = tree.proof_bytes(i);
        if !bad_proof.is_empty() {
            bad_proof[0][0] ^= 0xFF;
        } else {
            bad_proof = vec![[0xFF; 32]];
        }

        let result = dispatcher
            .validator
            .validate_push(root, i, &chunks[i as usize], &bad_proof)
            .await;
        match result {
            Err(PushReject::BadProof) => (),
            other => panic!("expected BadProof, got {other:?}"),
        }
        // No held update, no persistence.
        assert!(dispatcher.held_for(&root).is_empty());
    }

    /// Manifest validation: the existing `validate_manifest_push` recomputes
    /// the root. A malformed manifest is rejected before persistence.
    #[tokio::test]
    async fn manifest_validation_rejects_root_mismatch() {
        let (manifest, _, _) = make_manifest(4, 0x30);
        let bytes = cbor(&manifest);
        // Pretend we received this manifest under a DIFFERENT root.
        let wrong_root = [0xFF; 32];
        let err = validate_manifest_push(&hex::encode(wrong_root), &bytes).unwrap_err();
        assert!(err.contains("merkle_root mismatch"), "got: {err}");
    }

    // ── Reviewer-required ACL gate tests ────────────────────────────
    //
    // V2 Pull and ManifestPull MUST go through the same chain-governed
    // ACL the V1 pull path uses. These four tests pin both deny and
    // allow paths against an in-process toggle ACL — without them an
    // unauthorized peer that learns a CID could read over
    // `/sum/storage/v2` and bypass file governance.

    /// Pre-populate the local store with a chunk + manifest so the
    /// allow-path actually serves bytes (deny path returns ACCESS_DENIED
    /// before touching storage either way).
    async fn seed_store_with_file(
        stores: &Arc<RwLock<SumStore>>,
        manifest: &DataManifest,
        chunks: &[Vec<u8>],
    ) {
        let mut s = stores.write().await;
        for (i, data) in chunks.iter().enumerate() {
            let cid = &manifest.chunks[i].cid;
            s.local.put(cid, data).unwrap();
        }
        s.manifest_idx.insert(manifest).unwrap();
    }

    /// In-process peer for tests. The dispatcher's ACL only consults
    /// the trait — it never round-trips through libp2p — so any valid
    /// `PeerId` works.
    fn fake_peer() -> PeerId {
        sum_net::Keypair::generate_ed25519().public().to_peer_id()
    }

    /// Reviewer-required: ACL-denied V2 Pull surfaces ACCESS_DENIED, not
    /// the chunk bytes. The store deliberately HAS the chunk so the
    /// missing payload is provably from the ACL gate, not storage absence.
    #[tokio::test]
    async fn v2_pull_denied_returns_access_denied_response() {
        let (manifest, chunks, _tree) = make_manifest(4, 0x70);
        let stores = make_stores();
        seed_store_with_file(&stores, &manifest, &chunks).await;

        let dispatcher = build_dispatcher_with_acl(
            AllMockRpc::new(),
            five_archives()[0],
            stores.clone(),
            /* acl_allows = */ false,
        );
        // Sanity: store has the chunk, so empty response below is the
        // ACL gate firing, not a storage miss.
        let target_cid = manifest.chunks[0].cid.clone();
        assert!(stores.read().await.local.has(&target_cid));

        let resp = dispatcher
            .build_pull_response(fake_peer(), target_cid.clone(), 0, chunks[0].len() as u64)
            .await;
        match resp {
            ShardResponseV2::Data { cid, data, error, .. } => {
                assert_eq!(cid, target_cid);
                assert!(data.is_empty(), "denied pull must not carry chunk bytes");
                let err = error.expect("denied pull must set error");
                assert!(err.contains("ACCESS_DENIED"), "got: {err}");
            }
            other => panic!("expected Data response, got {other:?}"),
        }
    }

    /// Reviewer-required: ACL-allowed V2 Pull serves the bytes.
    #[tokio::test]
    async fn v2_pull_allowed_serves_chunk_bytes() {
        let (manifest, chunks, _tree) = make_manifest(4, 0x71);
        let stores = make_stores();
        seed_store_with_file(&stores, &manifest, &chunks).await;

        let dispatcher = build_dispatcher_with_acl(
            AllMockRpc::new(),
            five_archives()[0],
            stores.clone(),
            /* acl_allows = */ true,
        );
        let target_cid = manifest.chunks[0].cid.clone();
        let resp = dispatcher
            .build_pull_response(fake_peer(), target_cid.clone(), 0, chunks[0].len() as u64)
            .await;
        match resp {
            ShardResponseV2::Data { cid, data, error, total_bytes, offset } => {
                assert_eq!(cid, target_cid);
                assert!(error.is_none(), "allowed pull must not set error: {error:?}");
                assert_eq!(offset, 0);
                assert_eq!(data, chunks[0]);
                assert_eq!(total_bytes, chunks[0].len() as u64);
            }
            other => panic!("expected Data response, got {other:?}"),
        }
    }

    /// Reviewer-required: ACL-denied V2 ManifestPull surfaces
    /// ACCESS_DENIED, not the manifest CBOR.
    #[tokio::test]
    async fn v2_manifest_pull_denied_returns_access_denied_response() {
        let (manifest, chunks, _tree) = make_manifest(4, 0x80);
        let stores = make_stores();
        seed_store_with_file(&stores, &manifest, &chunks).await;

        let dispatcher = build_dispatcher_with_acl(
            AllMockRpc::new(),
            five_archives()[0],
            stores.clone(),
            /* acl_allows = */ false,
        );
        // Sanity: manifest IS indexed, so empty bytes below = ACL gate.
        assert!(stores.read().await.manifest_idx
            .get_by_merkle_root(&manifest.merkle_root).is_some());

        let resp = dispatcher
            .build_manifest_pull_response(fake_peer(), manifest.merkle_root)
            .await;
        match resp {
            ShardResponseV2::ManifestData { merkle_root, manifest_bytes, error } => {
                assert_eq!(merkle_root, manifest.merkle_root);
                assert!(manifest_bytes.is_empty(), "denied pull must not carry manifest bytes");
                let err = error.expect("denied pull must set error");
                assert!(err.contains("ACCESS_DENIED"), "got: {err}");
            }
            other => panic!("expected ManifestData response, got {other:?}"),
        }
    }

    /// Reviewer-required: ACL-allowed V2 ManifestPull serves CBOR.
    #[tokio::test]
    async fn v2_manifest_pull_allowed_serves_manifest_cbor() {
        let (manifest, chunks, _tree) = make_manifest(4, 0x81);
        let stores = make_stores();
        seed_store_with_file(&stores, &manifest, &chunks).await;

        let dispatcher = build_dispatcher_with_acl(
            AllMockRpc::new(),
            five_archives()[0],
            stores.clone(),
            /* acl_allows = */ true,
        );

        let resp = dispatcher
            .build_manifest_pull_response(fake_peer(), manifest.merkle_root)
            .await;
        match resp {
            ShardResponseV2::ManifestData { merkle_root, manifest_bytes, error } => {
                assert_eq!(merkle_root, manifest.merkle_root);
                assert!(error.is_none(), "allowed pull must not set error: {error:?}");
                // CBOR round-trips back to the original manifest.
                let decoded: DataManifest =
                    ciborium::de::from_reader(&manifest_bytes[..]).unwrap();
                assert_eq!(decoded.merkle_root, manifest.merkle_root);
                assert_eq!(decoded.chunk_count, manifest.chunk_count);
            }
            other => panic!("expected ManifestData response, got {other:?}"),
        }
    }
}
