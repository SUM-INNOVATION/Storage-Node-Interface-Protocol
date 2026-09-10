//! The one inbound shard dispatcher.
//!
//! `run_listen` and `simple_serve_loop` both serve inbound shard traffic, and
//! until now each carried its own hand-maintained copy of the dispatch. They
//! had already drifted: `simple_serve_loop` has no `ShardRequestedV2` arm at
//! all, so every inbound V2 request fell into its catch-all, was logged, and
//! was never answered — the response channel sat in the swarm's pending map
//! until the 120s reaper, by which time the peer had already timed out.
//!
//! That is the argument for this module, and it is not tidiness. Every gate
//! this protocol is about to grow — WP-B's V1-push retirement, conflict-aware
//! ambiguity denial, V2.1's activation check — is another arm or another guard
//! on exactly this code. With two copies, each of those can land correctly in
//! one loop and silently not in the other, and the second loop is the one
//! nobody looks at. One dispatcher makes "did this gate apply everywhere?" a
//! question the compiler answers.
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
//! ## What is deliberately unchanged
//!
//! `run_listen`'s behaviour, byte for byte on the wire. No wire format
//! changes. V1 push is **not** retired here — it remains unauthenticated and
//! unauthorized, exactly as before, and retiring it is WP-B's work. This
//! commit moves code; it does not decide policy.

use std::sync::Arc;

use sum_net::{PeerId, ShardRequest, ShardRequestV2, ShardResponse, ShardResponseV2, SumNetEvent};
use sum_store::SumStore;
use sum_store::serve::{MANIFEST_REQUEST_PREFIX, RespondShard};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::inbound_v2::{AccessChecker, RespondNet};

/// The error a node without a V2 dispatcher returns. One string, one place —
/// both loops now emit the identical text, which they did not before.
pub const V2_DISABLED_ERROR: &str = "V2 disabled on this node";

/// The error a peer outside a file's ACL receives.
pub const ACCESS_DENIED_ERROR: &str = "ACCESS_DENIED: not in file access list";

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
}

impl ShardDispatch {
    pub fn new(
        store: Arc<RwLock<SumStore>>,
        acl: Arc<dyn AccessChecker>,
        v2: Option<Arc<dyn V2Handler>>,
    ) -> Self {
        Self { store, acl, v2 }
    }

    /// Handle one event if it is an inbound shard request.
    ///
    /// This is the entry point both loops call, and calling it is what makes
    /// them share a dispatch rather than merely resemble one.
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

    /// The V1 four-way dispatch.
    ///
    /// Unchanged in behaviour from `run_listen`'s copy:
    ///
    /// * **manifest push** — mutates the index, so it takes the write lock.
    ///   Without this branch an archive that received chunk pushes never learns
    ///   the `cid → root` mapping and production ACL denies pulls for those
    ///   CIDs. Still unauthenticated; see the module docs.
    /// * **chunk push** — read lock; `serve::handle_request` verifies the CID.
    /// * **pulls, manifest or chunk** — ACL gate, then read lock.
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

        match (is_manifest, is_push) {
            (true, true) => {
                let mut store_w = self.store.write().await;
                sum_store::serve::handle_manifest_push(
                    net,
                    &mut store_w.manifest_idx,
                    request,
                    channel_id,
                )
                .await;
            }
            (false, true) => {
                let store_read = self.store.read().await;
                sum_store::serve::handle_request(
                    net,
                    &store_read.local,
                    &store_read.manifest_idx,
                    request,
                    channel_id,
                )
                .await;
            }
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
    /// Dropping the event — which is what `simple_serve_loop` did — leaves the
    /// response channel pending until the swarm's 120s reaper and gives the
    /// peer a timeout instead of a reject.
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
    /// left to time out, which is precisely the defect `simple_serve_loop` had.
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

    #[tokio::test]
    async fn a_chunk_push_is_stored_without_consulting_the_acl() {
        let body = b"pushed bytes";
        let cid = sum_store::content_id::cid_from_data(body);
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();

        // `allow = false`: a push must not be gated by the pull ACL. This
        // pins current behaviour, which WP-B changes by retiring the path —
        // not by adding a gate here.
        dispatch(store.clone(), false, None)
            .on_v1_request(&net, &PeerId::random(), &push(&cid, body.to_vec()), 3)
            .await;

        assert_eq!(net.v1_count(), 1);
        assert_eq!(net.last_v1().1.error, None);
        assert_eq!(store.read().await.local.get(&cid).unwrap(), body);
    }

    #[tokio::test]
    async fn a_manifest_push_is_indexed() {
        let m = well_formed(&[b"alpha", b"beta"]);
        let root_hex = hex::encode(m.merkle_root);
        let (_d, store) = store_with(&[]);
        let net = Recorder::default();

        dispatch(store.clone(), false, None)
            .on_v1_request(
                &net,
                &PeerId::random(),
                &push(&format!("{MANIFEST_REQUEST_PREFIX}{root_hex}"), cbor(&m)),
                4,
            )
            .await;

        assert_eq!(net.v1_count(), 1);
        assert_eq!(net.last_v1().1.error, None);
        assert!(
            store
                .read()
                .await
                .manifest_idx
                .get_by_merkle_root(&m.merkle_root)
                .is_some()
        );
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

    /// `on_event` is the entry point both loops call. Everything else falls
    /// through untouched, so neither loop has to re-derive which variants are
    /// shard traffic.
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
