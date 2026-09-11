//! The one inbound shard dispatcher.
//!
//! `run_listen` is the one serve loop, and every inbound shard request it
//! receives — V1 and V2 — is decided here.
//!
//! There used to be a second. `simple_serve_loop` served V1 ingest's node mode
//! and carried its own hand-maintained copy of this dispatch, which had already
//! drifted: it never grew a `ShardRequestedV2` arm, so every inbound V2 request
//! fell into its catch-all, was logged, and was never answered — the response
//! channel sat in the swarm's pending map until the 120s reaper, by which time
//! the peer had already timed out. This module was introduced to end that
//! duplication; WP-B then retired V1 ingest, which left that loop with no
//! caller, and it was removed.
//!
//! The argument for keeping the dispatch here outlives the second loop, and it
//! is not tidiness. Every gate this protocol is about to grow — conflict-aware
//! ambiguity denial, V2.1's activation check — is another arm or another guard
//! on exactly this code. A copy of it is a place a gate can be forgotten, and
//! the forgotten copy is the one nobody reads. One dispatcher makes "did this
//! gate apply everywhere?" a question the compiler answers, and
//! `tests/shared_dispatch_wiring.rs` fails the build if a second copy returns.
//!
//! ## What is centralised
//!
//! Every decision an inbound shard request can turn on: the V1 four-way
//! dispatch (manifest push / chunk push / manifest pull / chunk pull), the ACL
//! gate on pulls, the V2 hand-off, the structured refusal a node without a V2
//! dispatcher must send, and — the property that ties them together — that
//! **every path answers exactly once**. An unanswered request is a leaked
//! response channel and a peer that waits out the protocol timeout.
//!
//! ## V1 push is retired here
//!
//! Both V1 push forms — a chunk push (`push_data: Some(..)` under a plain CID)
//! and a manifest push (the same under a `manifest:` CID) — are refused, and
//! refused **first**: before the store lock is taken, before the ACL is
//! consulted, and before `sum_store::serve` is reached. The ordering is the
//! security property, not a performance one. A V1 push carries no Merkle
//! proof, no assignment, and no signature; the receiving side could only ever
//! check that the sender hashed what it sent. Letting such a request as far as
//! a lock means an unauthenticated peer can make this node contend for one,
//! and letting it as far as `serve` means it can write.
//!
//! The refusal is a stable typed value — [`v1_push_retired_response`], carrying
//! [`V1_PUSH_RETIRED_ERROR`] — so a peer gets an immediate, greppable answer
//! naming the replacement, rather than a timeout.
//!
//! V1 **pull** is untouched: `/sum/storage/v1` reads keep working, ACL gate and
//! all, and the node now routes V1 pulls on a V1-only behaviour so V1-only
//! peers stay reachable.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sum_net::{PeerId, ShardRequest, ShardRequestV2, ShardResponse, ShardResponseV2, SumNetEvent};
use sum_store::SumStore;
use sum_store::serve::{MANIFEST_REQUEST_PREFIX, RespondShard};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::inbound_v2::{AccessChecker, RespondNet};

/// The error a node without a V2 dispatcher returns. One string, one place —
/// One definition, so the text cannot drift between call sites. When there
/// were two serve loops only one of them emitted this at all.
pub const V2_DISABLED_ERROR: &str = "V2 disabled on this node";

/// The error a peer outside a file's ACL receives.
pub const ACCESS_DENIED_ERROR: &str = "ACCESS_DENIED: not in file access list";

/// The stable refusal a V1 push receives. One string, matched exactly by the
/// regression tests, and naming the replacement so an operator reading a peer
/// log knows what to do rather than only what failed.
///
/// The prefix is a machine-readable code; peers should match on it and not on
/// the prose.
pub const V1_PUSH_RETIRED_ERROR: &str = "V1_PUSH_RETIRED: /sum/storage/v1 push is retired — it carries no Merkle \
     proof and no assignment. Use /sum/storage/v2 Push (chunk) or ManifestPush \
     (manifest).";

/// Fixed-cardinality refusal counters for the inbound dispatcher.
///
/// Fields, not a map. A keyed counter here would be keyed by something a
/// remote peer chooses — its peer id, or the CID it asked for — and an
/// unauthenticated peer that can name a metric key can grow the metric set
/// without bound. There are exactly two things to count and they are exactly
/// two fields.
#[derive(Debug, Default)]
pub struct RefusalCounters {
    v1_chunk_push: AtomicU64,
    v1_manifest_push: AtomicU64,
}

/// A snapshot of [`RefusalCounters`], for tests and for whatever exports
/// metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefusalCounts {
    /// Inbound V1 chunk pushes refused.
    pub v1_chunk_push: u64,
    /// Inbound V1 manifest pushes refused.
    pub v1_manifest_push: u64,
}

impl RefusalCounters {
    fn snapshot(&self) -> RefusalCounts {
        RefusalCounts {
            v1_chunk_push: self.v1_chunk_push.load(Ordering::Relaxed),
            v1_manifest_push: self.v1_manifest_push.load(Ordering::Relaxed),
        }
    }
}

/// The refusal sent for either V1 push form.
///
/// Shaped as an ordinary `ShardResponse` with a non-empty `error`, which is the
/// only failure shape the V1 wire has. `total_bytes` and `data` are empty: this
/// is a refusal, not a partial result.
pub fn v1_push_retired_response(request: &ShardRequest) -> ShardResponse {
    ShardResponse {
        cid: request.cid.clone(),
        offset: 0,
        total_bytes: 0,
        data: Vec::new(),
        error: Some(V1_PUSH_RETIRED_ERROR.into()),
    }
}

/// Anything that can answer both protocol versions.
///
/// `SumNet` satisfies it; so does a test recorder. Blanket-implemented so no
/// type has to name it.
pub trait ShardResponder: RespondShard + RespondNet {}
impl<T: RespondShard + RespondNet + ?Sized> ShardResponder for T {}

/// The V2 hand-off, behind a trait.
///
/// `V2Dispatcher<V, A, T>` is generic over three RPC clients, so it cannot be
/// stored as a trait object directly and cannot be built in a unit test
/// without standing up chain plumbing. This is the dyn-safe seam: production
/// passes the real dispatcher, tests pass a recorder, and both go through the
/// same routing code.
#[async_trait::async_trait]
pub trait V2Handler: Send + Sync {
    async fn handle(
        &self,
        net: &dyn RespondNet,
        peer_id: PeerId,
        request: ShardRequestV2,
        channel_id: u64,
    );
}

/// Whether the dispatcher consumed the event.
///
/// Returned so a caller can fall through to its own handling for everything
/// that is not an inbound shard request, without either loop having to
/// re-derive which variants those are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The dispatcher answered this event.
    Yes,
    /// Not an inbound shard request. The caller owns it.
    No,
}

/// Shared inbound shard dispatch.
pub struct ShardDispatch {
    store: Arc<RwLock<SumStore>>,
    acl: Arc<dyn AccessChecker>,
    /// `None` on a node with no signing key, which cannot serve V2.
    v2: Option<Arc<dyn V2Handler>>,
    /// Fixed-field refusal counters. See [`RefusalCounters`].
    refusals: RefusalCounters,
}

impl ShardDispatch {
    pub fn new(
        store: Arc<RwLock<SumStore>>,
        acl: Arc<dyn AccessChecker>,
        v2: Option<Arc<dyn V2Handler>>,
    ) -> Self {
        Self {
            store,
            acl,
            v2,
            refusals: RefusalCounters::default(),
        }
    }

    /// Snapshot of the refusal counters.
    pub fn refusals(&self) -> RefusalCounts {
        self.refusals.snapshot()
    }

    /// Handle one event if it is an inbound shard request.
    ///
    /// The entry point `run_listen` calls. Any future serve loop calls this
    /// too rather than growing its own copy — sharing a dispatch, not merely
    /// resembling one.
    pub async fn on_event<N>(&self, net: &N, event: &SumNetEvent) -> Handled
    where
        N: RespondShard + RespondNet,
    {
        match event {
            SumNetEvent::ShardRequested {
                peer_id,
                request,
                channel_id,
            } => {
                self.on_v1_request(net, peer_id, request, *channel_id).await;
                Handled::Yes
            }
            SumNetEvent::ShardRequestedV2 {
                peer_id,
                request,
                channel_id,
            } => {
                self.on_v2_request(net, *peer_id, request, *channel_id)
                    .await;
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    /// The V1 dispatch: two refusals and two pulls.
    ///
    /// * **any push, chunk or manifest** — refused with
    ///   [`V1_PUSH_RETIRED_ERROR`]. This arm comes first and returns before
    ///   `self.store` is touched, so no unauthenticated peer can make this node
    ///   take a lock, and `sum_store::serve` is never reached for a push.
    /// * **pulls, manifest or chunk** — unchanged: ACL gate, then read lock.
    pub async fn on_v1_request<N>(
        &self,
        net: &N,
        peer_id: &PeerId,
        request: &ShardRequest,
        channel_id: u64,
    ) where
        N: RespondShard + ?Sized,
    {
        let is_manifest = request.cid.starts_with(MANIFEST_REQUEST_PREFIX);
        let is_push = request.push_data.is_some();

        // FIRST. Before `self.store` is read or written, and before any
        // `sum_store::serve` entry point. Moving this below either of those is
        // the mutation `an_inbound_v1_*_push_never_reaches_the_store` catches.
        if is_push {
            if is_manifest {
                self.refusals
                    .v1_manifest_push
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                self.refusals.v1_chunk_push.fetch_add(1, Ordering::Relaxed);
            }
            warn!(
                peer = %peer_id,
                cid = %request.cid,
                bytes = request.push_data.as_ref().map_or(0, Vec::len),
                manifest = is_manifest,
                "refused retired V1 push"
            );
            let _ = net
                .respond_shard(channel_id, v1_push_retired_response(request))
                .await;
            return;
        }

        match (is_manifest, is_push) {
            (_, true) => unreachable!("every push returned above"),
            (_, false) => {
                let store_read = self.store.read().await;
                let allowed = self
                    .acl
                    .check_access_or_default(peer_id, &request.cid, &store_read.manifest_idx)
                    .await;
                if allowed {
                    sum_store::serve::handle_request(
                        net,
                        &store_read.local,
                        &store_read.manifest_idx,
                        request,
                        channel_id,
                    )
                    .await;
                } else {
                    info!(
                        peer = %peer_id,
                        cid = %request.cid,
                        "ACCESS DENIED — peer not in file ACL"
                    );
                    let resp = ShardResponse {
                        cid: request.cid.clone(),
                        offset: 0,
                        total_bytes: 0,
                        data: Vec::new(),
                        error: Some(ACCESS_DENIED_ERROR.into()),
                    };
                    let _ = net.respond_shard(channel_id, resp).await;
                }
            }
        }
    }

    /// The V2 hand-off, or the structured refusal when V2 is disabled.
    ///
    /// The refusal matters: a node with no signing key must still *answer*.
    /// Dropping the event — which is what the retired `simple_serve_loop` did —
    /// left the response channel pending until the swarm's 120s reaper and gave
    /// the peer a timeout instead of a reject.
    pub async fn on_v2_request<N>(
        &self,
        net: &N,
        peer_id: PeerId,
        request: &ShardRequestV2,
        channel_id: u64,
    ) where
        N: RespondNet,
    {
        if let Some(v2) = &self.v2 {
            v2.handle(net, peer_id, request.clone(), channel_id).await;
            return;
        }

        warn!(
            %peer_id,
            channel_id,
            "V2 request received but V2 dispatcher disabled — node has no signing key"
        );
        let resp = v2_disabled_response(request);
        let _ = net.respond_shard_v2(channel_id, resp).await;
    }
}

/// The structurally-valid V2 error for a node that cannot serve V2.
///
/// Exhaustively matched, so a new `ShardRequestV2` variant is a compile error
/// here rather than a silent unanswered request.
pub fn v2_disabled_response(request: &ShardRequestV2) -> ShardResponseV2 {
    match request {
        ShardRequestV2::Pull { cid, offset, .. } => ShardResponseV2::Data {
            cid: cid.clone(),
            offset: *offset,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(V2_DISABLED_ERROR.into()),
        },
        ShardRequestV2::Push {
            merkle_root,
            chunk_index,
            ..
        } => ShardResponseV2::PushAck {
            merkle_root: *merkle_root,
            chunk_index: *chunk_index,
            error: Some(V2_DISABLED_ERROR.into()),
        },
        ShardRequestV2::ManifestPush { merkle_root, .. } => ShardResponseV2::ManifestPushAck {
            merkle_root: *merkle_root,
            error: Some(V2_DISABLED_ERROR.into()),
        },
        ShardRequestV2::ManifestPull { merkle_root } => ShardResponseV2::ManifestData {
            merkle_root: *merkle_root,
            manifest_bytes: Vec::new(),
            error: Some(V2_DISABLED_ERROR.into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use sum_types::config::StoreConfig;
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    // ── Recorders ────────────────────────────────────────────────────────

    /// Captures every response, on both protocol versions, in arrival order.
    ///
    /// Counting responses is what makes "every path answers exactly once"
    /// testable — an unanswered request is a leaked response channel and a peer
    /// left to time out, which is precisely the defect the retired
    /// `simple_serve_loop` had.
    #[derive(Default)]
    struct Recorder {
        v1: Mutex<Vec<(u64, ShardResponse)>>,
        v2: Mutex<Vec<(u64, ShardResponseV2)>>,
    }

    impl Recorder {
        fn v1_count(&self) -> usize {
            self.v1.lock().unwrap().len()
        }
        fn v2_count(&self) -> usize {
            self.v2.lock().unwrap().len()
        }
        fn total(&self) -> usize {
            self.v1_count() + self.v2_count()
        }
        fn last_v1(&self) -> (u64, ShardResponse) {
            self.v1.lock().unwrap().last().cloned().expect("a V1 reply")
        }
    }

    #[async_trait::async_trait]
    impl RespondShard for Recorder {
        async fn respond_shard(
            &self,
            channel_id: u64,
            response: ShardResponse,
        ) -> anyhow::Result<()> {
            self.v1.lock().unwrap().push((channel_id, response));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl RespondNet for Recorder {
        async fn respond_shard_v2(
            &self,
            channel_id: u64,
            response: ShardResponseV2,
        ) -> anyhow::Result<()> {
            self.v2.lock().unwrap().push((channel_id, response));
            Ok(())
        }
    }

    /// Records which V2 requests reached the dispatcher, and answers them so
    /// the response-count invariant holds on this path too.
    #[derive(Default)]
    struct V2Recorder {
        seen: Mutex<Vec<(PeerId, ShardRequestV2, u64)>>,
    }

    #[async_trait::async_trait]
    impl V2Handler for V2Recorder {
        async fn handle(
            &self,
            net: &dyn RespondNet,
            peer_id: PeerId,
            request: ShardRequestV2,
            channel_id: u64,
        ) {
            self.seen
                .lock()
                .unwrap()
                .push((peer_id, request.clone(), channel_id));
            let _ = net
                .respond_shard_v2(channel_id, v2_disabled_response(&request))
                .await;
        }
    }

    /// An ACL with a fixed answer, so denial is exercised without chain
    /// plumbing.
    struct FixedAcl(bool);

    #[async_trait::async_trait]
    impl AccessChecker for FixedAcl {
        async fn check_access_or_default(
            &self,
            _peer_id: &PeerId,
            _cid: &str,
            _manifest_index: &sum_store::ManifestIndex,
        ) -> bool {
            self.0
        }
    }

    // ── Fixtures ─────────────────────────────────────────────────────────

    fn store_with(chunks: &[&[u8]]) -> (tempfile::TempDir, Arc<RwLock<SumStore>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = SumStore::new(StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024 * 1024,
        })
        .unwrap();
        for c in chunks {
            store
                .local
                .put(sum_store::content_id::cid_from_data(c).as_str(), c)
                .unwrap();
        }
        (dir, Arc::new(RwLock::new(store)))
    }

    /// A genuinely well-formed manifest, so a push is accepted for the right
    /// reason rather than by a relaxed check.
    fn well_formed(bodies: &[&[u8]]) -> DataManifest {
        let mut chunks = Vec::new();
        let mut leaves = Vec::new();
        let mut offset = 0u64;
        for (i, body) in bodies.iter().enumerate() {
            let hash = blake3::hash(body);
            leaves.push(hash);
            chunks.push(ChunkDescriptor {
                chunk_index: i as u32,
                offset,
                size: body.len() as u64,
                blake3_hash: *hash.as_bytes(),
                cid: sum_store::content_id::cid_from_blake3_hash(&hash),
                plaintext_blake3_hash: None,
            });
            offset += body.len() as u64;
        }
        DataManifest {
            file_name: "fixture.bin".into(),
            file_hash: [0xAB; 32],
            total_size_bytes: offset,
            chunk_count: chunks.len() as u32,
            merkle_root: *sum_store::merkle::MerkleTree::build(&leaves)
                .root()
                .as_bytes(),
            chunks,
        }
    }

    fn cbor(m: &DataManifest) -> Vec<u8> {
        let mut b = Vec::new();
        ciborium::ser::into_writer(m, &mut b).unwrap();
        b
    }

    fn pull(cid: &str) -> ShardRequest {
        ShardRequest {
            cid: cid.to_string(),
            offset: None,
            max_bytes: None,
            push_data: None,
        }
    }

    fn push(cid: &str, data: Vec<u8>) -> ShardRequest {
        ShardRequest {
            cid: cid.to_string(),
            offset: None,
            max_bytes: None,
            push_data: Some(data),
        }
    }

    fn dispatch(
        store: Arc<RwLock<SumStore>>,
        allow: bool,
        v2: Option<Arc<dyn V2Handler>>,
    ) -> ShardDispatch {
        ShardDispatch::new(store, Arc::new(FixedAcl(allow)), v2)
    }

    fn all_v2_variants() -> Vec<ShardRequestV2> {
        vec![
            ShardRequestV2::Pull {
                cid: "some-cid".into(),
                offset: 7,
                max_bytes: 99,
            },
            ShardRequestV2::Push {
                data: vec![1, 2, 3],
                merkle_root: [0x11; 32],
                chunk_index: 4,
                merkle_path: Vec::new(),
            },
            ShardRequestV2::ManifestPush {
                merkle_root: [0x22; 32],
                manifest_bytes: vec![9],
            },
            ShardRequestV2::ManifestPull {
                merkle_root: [0x33; 32],
            },
        ]
    }

    // ── V1 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_permitted_chunk_pull_is_served() {
        let body = b"the chunk";
        let cid = sum_store::content_id::cid_from_data(body);
        let (_d, store) = store_with(&[body]);
        let net = Recorder::default();

        dispatch(store, true, None)
            .on_v1_request(&net, &PeerId::random(), &pull(&cid), 1)
            .await;

        assert_eq!(net.v1_count(), 1, "exactly one reply");
        let (ch, resp) = net.last_v1();
        assert_eq!(ch, 1);
        assert_eq!(resp.error, None, "{:?}", resp.error);
        assert_eq!(resp.data, body);
    }

    #[tokio::test]
    async fn a_denied_pull_is_refused_and_still_answered() {
        let body = b"the chunk";
        let cid = sum_store::content_id::cid_from_data(body);
        let (_d, store) = store_with(&[body]);
        let net = Recorder::default();

        dispatch(store, false, None)
            .on_v1_request(&net, &PeerId::random(), &pull(&cid), 2)
            .await;

        assert_eq!(net.v1_count(), 1, "a denial is still a reply");
        let (ch, resp) = net.last_v1();
        assert_eq!(ch, 2);
        assert_eq!(resp.error.as_deref(), Some(ACCESS_DENIED_ERROR));
        assert!(resp.data.is_empty(), "a denial carries no bytes");
        assert_eq!(resp.total_bytes, 0);
    }

    // ── V1 push retirement ───────────────────────────────────────────────

    /// An inbound V1 chunk push is refused, answered once, and — the part that
    /// matters — never written.
    ///
    /// `allow = false` is not doing the work here: V1 push was never ACL-gated,
    /// so a store that stays empty proves the refusal and not the ACL.
    #[tokio::test]
    async fn an_inbound_v1_chunk_push_is_refused_and_never_reaches_the_store() {
        let body = b"pushed bytes";
        let cid = sum_store::content_id::cid_from_data(body);
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();
        let d = dispatch(store.clone(), true, None);

        d.on_v1_request(&net, &PeerId::random(), &push(&cid, body.to_vec()), 3)
            .await;

        assert_eq!(net.v1_count(), 1, "a refusal is still exactly one reply");
        let (ch, resp) = net.last_v1();
        assert_eq!(ch, 3);
        assert_eq!(
            resp.error.as_deref(),
            Some(V1_PUSH_RETIRED_ERROR),
            "the refusal must be the stable typed one"
        );
        assert_eq!(resp.cid, cid, "the refusal names what was refused");
        assert!(resp.data.is_empty());
        assert_eq!(resp.total_bytes, 0);

        assert!(
            store.read().await.local.get(&cid).is_err(),
            "the pushed bytes must not have been written"
        );
        assert_eq!(
            d.refusals(),
            RefusalCounts {
                v1_chunk_push: 1,
                v1_manifest_push: 0
            }
        );
    }

    /// The manifest form of the same push, refused the same way. Both forms,
    /// because they took different branches — and different locks — before.
    #[tokio::test]
    async fn an_inbound_v1_manifest_push_is_refused_and_never_reaches_the_index() {
        let m = well_formed(&[b"alpha", b"beta"]);
        let root_hex = hex::encode(m.merkle_root);
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();
        let d = dispatch(store.clone(), true, None);

        d.on_v1_request(
            &net,
            &PeerId::random(),
            &push(&format!("{MANIFEST_REQUEST_PREFIX}{root_hex}"), cbor(&m)),
            4,
        )
        .await;

        assert_eq!(net.v1_count(), 1);
        assert_eq!(
            net.last_v1().1.error.as_deref(),
            Some(V1_PUSH_RETIRED_ERROR)
        );
        assert!(
            store
                .read()
                .await
                .manifest_idx
                .get_by_merkle_root(&m.merkle_root)
                .is_none(),
            "a well-formed manifest must still not be indexed from a V1 push"
        );
        assert_eq!(
            d.refusals(),
            RefusalCounts {
                v1_chunk_push: 0,
                v1_manifest_push: 1
            }
        );
    }

    /// The refusal happens **before the store lock is taken**. Holding the
    /// write lock for the whole call would deadlock any handler that reaches
    /// for the store; the refusal completes regardless, which is only possible
    /// if it never asks for the lock.
    ///
    /// This is the ordering assertion. Moving the push check below
    /// `self.store.read()`/`.write()` hangs this test rather than failing it,
    /// so it is bounded by an explicit timeout and the timeout is the failure.
    #[tokio::test]
    async fn a_v1_push_is_refused_before_the_store_lock_is_taken() {
        let body = b"pushed bytes";
        let cid = sum_store::content_id::cid_from_data(body);
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();
        let d = dispatch(store.clone(), true, None);

        // Hold the write lock for the duration of the call.
        let guard = store.write().await;

        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            d.on_v1_request(&net, &PeerId::random(), &push(&cid, body.to_vec()), 9),
        )
        .await;

        assert!(
            refused.is_ok(),
            "the refusal waited on the store lock — it must be decided before any lock"
        );
        drop(guard);
        assert_eq!(
            net.last_v1().1.error.as_deref(),
            Some(V1_PUSH_RETIRED_ERROR)
        );
    }

    /// The refusal text is the contract a peer reads. It names a machine
    /// prefix and the replacement, so an operator seeing it in a log knows
    /// what to run.
    #[test]
    fn the_v1_push_refusal_names_its_replacement() {
        assert!(V1_PUSH_RETIRED_ERROR.starts_with("V1_PUSH_RETIRED:"));
        assert!(V1_PUSH_RETIRED_ERROR.contains("/sum/storage/v2"));
        assert!(V1_PUSH_RETIRED_ERROR.contains("Push"));
        assert!(V1_PUSH_RETIRED_ERROR.contains("ManifestPush"));
    }

    /// Pull and push are told apart by `push_data`, not by the CID. An empty
    /// `push_data` is still a push — `Some(vec![])` — and must be refused, or
    /// the retirement has a zero-length hole in it.
    #[tokio::test]
    async fn an_empty_v1_push_is_still_a_push() {
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();
        let d = dispatch(store, true, None);

        d.on_v1_request(&net, &PeerId::random(), &push("any-cid", Vec::new()), 11)
            .await;

        assert_eq!(
            net.last_v1().1.error.as_deref(),
            Some(V1_PUSH_RETIRED_ERROR)
        );
        assert_eq!(d.refusals().v1_chunk_push, 1);
    }

    #[tokio::test]
    async fn a_manifest_pull_is_acl_gated_like_a_chunk_pull() {
        let m = well_formed(&[b"alpha"]);
        let root_hex = hex::encode(m.merkle_root);
        let cid = format!("{MANIFEST_REQUEST_PREFIX}{root_hex}");
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();

        dispatch(store, false, None)
            .on_v1_request(&net, &PeerId::random(), &pull(&cid), 5)
            .await;

        assert_eq!(net.v1_count(), 1);
        assert_eq!(net.last_v1().1.error.as_deref(), Some(ACCESS_DENIED_ERROR));
    }

    // ── V2 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn every_v2_variant_reaches_the_dispatcher_when_one_exists() {
        let (_d, store) = store_with(&[]);
        let v2 = Arc::new(V2Recorder::default());
        let d = dispatch(store, true, Some(v2.clone()));
        let net = Recorder::default();
        let peer = PeerId::random();

        for (i, req) in all_v2_variants().into_iter().enumerate() {
            d.on_v2_request(&net, peer, &req, 100 + i as u64).await;
        }

        let seen = v2.seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "all four variants must be routed");
        for (i, (p, _, ch)) in seen.iter().enumerate() {
            assert_eq!(*p, peer, "the peer must be forwarded unchanged");
            assert_eq!(
                *ch,
                100 + i as u64,
                "the channel must be forwarded unchanged"
            );
        }
        assert_eq!(net.v2_count(), 4, "one reply per request");
    }

    /// The defect this module exists to close: a node with no V2 dispatcher
    /// must *answer*, not drop. Dropping leaves the response channel pending
    /// until the swarm's reaper and gives the peer a timeout.
    #[tokio::test]
    async fn a_v2_disabled_node_answers_every_variant() {
        let (_d, store) = store_with(&[]);
        let d = dispatch(store, true, None);
        let net = Recorder::default();
        let peer = PeerId::random();

        for (i, req) in all_v2_variants().into_iter().enumerate() {
            d.on_v2_request(&net, peer, &req, 200 + i as u64).await;
        }

        assert_eq!(net.v2_count(), 4, "every variant must be answered");
        let replies = net.v2.lock().unwrap();

        // Shape-matched to the request, and every one carries the error.
        assert!(matches!(
            &replies[0].1,
            ShardResponseV2::Data { cid, offset, total_bytes, data, error }
                if cid == "some-cid" && *offset == 7 && *total_bytes == 0
                    && data.is_empty() && error.as_deref() == Some(V2_DISABLED_ERROR)
        ));
        assert!(matches!(
            &replies[1].1,
            ShardResponseV2::PushAck { merkle_root, chunk_index, error }
                if *merkle_root == [0x11; 32] && *chunk_index == 4
                    && error.as_deref() == Some(V2_DISABLED_ERROR)
        ));
        assert!(matches!(
            &replies[2].1,
            ShardResponseV2::ManifestPushAck { merkle_root, error }
                if *merkle_root == [0x22; 32] && error.as_deref() == Some(V2_DISABLED_ERROR)
        ));
        assert!(matches!(
            &replies[3].1,
            ShardResponseV2::ManifestData { merkle_root, manifest_bytes, error }
                if *merkle_root == [0x33; 32] && manifest_bytes.is_empty()
                    && error.as_deref() == Some(V2_DISABLED_ERROR)
        ));
        for (i, (ch, _)) in replies.iter().enumerate() {
            assert_eq!(*ch, 200 + i as u64);
        }
    }

    // ── Routing and channel accounting ───────────────────────────────────

    /// `on_event` is the serve loop's entry point. Everything else falls
    /// through untouched, so no caller has to re-derive which event variants
    /// are shard traffic.
    #[tokio::test]
    async fn on_event_claims_shard_traffic_and_nothing_else() {
        let (_d, store) = store_with(&[]);
        let d = dispatch(store, true, None);
        let net = Recorder::default();

        let claimed = d
            .on_event(
                &net,
                &SumNetEvent::ShardRequestedV2 {
                    peer_id: PeerId::random(),
                    request: ShardRequestV2::ManifestPull {
                        merkle_root: [0x44; 32],
                    },
                    channel_id: 9,
                },
            )
            .await;
        assert_eq!(claimed, Handled::Yes);
        assert_eq!(net.v2_count(), 1);

        let not_claimed = d
            .on_event(
                &net,
                &SumNetEvent::PeerConnected {
                    peer_id: PeerId::random(),
                },
            )
            .await;
        assert_eq!(not_claimed, Handled::No);
        assert_eq!(net.total(), 1, "a non-shard event must not be answered");
    }

    /// The response-channel invariant, over every request shape at once.
    ///
    /// A channel is cleaned up by being answered; an unanswered request leaks
    /// it until the reaper. So "exactly one reply per request, on the channel
    /// it arrived on" is the testable form of channel cleanup.
    #[tokio::test]
    async fn every_request_shape_answers_exactly_once_on_its_own_channel() {
        let body = b"chunk";
        let cid = sum_store::content_id::cid_from_data(body);
        let m = well_formed(&[b"alpha"]);
        let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", hex::encode(m.merkle_root));
        let (_d, store) = store_with(&[body]);
        let d = dispatch(store, true, None);
        let net = Recorder::default();
        let peer = PeerId::random();

        let v1: Vec<ShardRequest> = vec![
            pull(&cid),
            pull(&manifest_cid),
            push(&cid, body.to_vec()),
            push(&manifest_cid, cbor(&m)),
            pull("bafkr4ianonexistentcidthatisnotheldbythisnodeatallxxxxxxxxxxxxx"),
        ];
        let mut expected = 0usize;
        for (i, req) in v1.iter().enumerate() {
            d.on_v1_request(&net, &peer, req, 300 + i as u64).await;
            expected += 1;
            assert_eq!(
                net.total(),
                expected,
                "shape {i} did not answer exactly once"
            );
        }
        for (i, req) in all_v2_variants().iter().enumerate() {
            d.on_v2_request(&net, peer, req, 400 + i as u64).await;
            expected += 1;
            assert_eq!(net.total(), expected, "V2 variant {i} did not answer once");
        }

        // Every channel answered exactly once, and none twice.
        let mut channels: Vec<u64> = net
            .v1
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| *c)
            .chain(net.v2.lock().unwrap().iter().map(|(c, _)| *c))
            .collect();
        channels.sort_unstable();
        let unique = {
            let mut c = channels.clone();
            c.dedup();
            c
        };
        assert_eq!(channels, unique, "a channel was answered more than once");
        assert_eq!(channels.len(), expected);
    }

    /// A pull for a CID this node does not hold is still answered — with an
    /// error, not with silence.
    #[tokio::test]
    async fn an_unheld_chunk_pull_is_answered_with_an_error() {
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();
        dispatch(store, true, None)
            .on_v1_request(
                &net,
                &PeerId::random(),
                &pull("bafkr4iaabsentcidxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
                6,
            )
            .await;
        assert_eq!(net.v1_count(), 1);
        assert!(net.last_v1().1.error.is_some());
    }
}
