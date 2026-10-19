//! Listen-mode transport for the V2 re-fetch ([`crate::refetch_v2`]):
//! outbound pulls from other archives, addressed by chain address, answered
//! through the listen loop.
//!
//! **Correlation.** The networking layer already ties every outbound V2
//! request to the request this node built (`sum_net::correlation`): a
//! `ShardReceivedV2` or `ShardRequestFailed` event carries the
//! [`OutboundOrigin`], the peer plus exactly what was asked (`V2Pull { cid }`
//! or `V2ManifestPull { merkle_root }`). This transport keeps one waiter per
//! (peer, request) and the listen loop hands it those events through
//! [`ListenRefetchTransport::on_event`]. Nothing is matched on fields the peer
//! wrote, and an event that answers no waiter is left to the listen loop.
//!
//! **Addressing.** A chain address resolves to a connected peer whose L1
//! identity the listen loop has mapped from `PeerIdentified`. An archive that
//! is not connected and identified is reported unreachable, and the re-fetch
//! tries its next source. There is no dialing and no new wire message: a pull
//! is the existing V2 `Pull` / `ManifestPull`, served by the other archive's
//! existing handler and ACL.
//!
//! **Answers.** A chunk is pulled in one window (`offset 0`, `max_bytes` =
//! the manifest's size for it). A refusal, a partial or offset answer, an
//! answer of the wrong variant, a failure event and silence past the timeout
//! are all errors. The bytes are verified by the caller, not here.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use sum_net::{OutboundOrigin, OutboundRequestKind, PeerId, ShardResponseV2, SumNet, SumNetEvent};
use tokio::sync::{RwLock, oneshot};

use crate::refetch_v2::RefetchTransport;

/// How long one pull waits for its answer.
pub const DEFAULT_PULL_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends V2 pulls: [`SumNet`] in production, a recorder in tests.
#[async_trait]
pub trait V2PullSender: Send + Sync {
    async fn pull_chunk_v2(
        &self,
        peer: PeerId,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) -> Result<()>;
    async fn pull_manifest_v2(&self, peer: PeerId, merkle_root: [u8; 32]) -> Result<()>;
}

#[async_trait]
impl V2PullSender for SumNet {
    async fn pull_chunk_v2(
        &self,
        peer: PeerId,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) -> Result<()> {
        SumNet::pull_chunk_v2(self, peer, cid, offset, max_bytes).await
    }
    async fn pull_manifest_v2(&self, peer: PeerId, merkle_root: [u8; 32]) -> Result<()> {
        SumNet::pull_manifest_v2(self, peer, merkle_root).await
    }
}

#[async_trait]
impl<T: V2PullSender + ?Sized> V2PullSender for Arc<T> {
    async fn pull_chunk_v2(
        &self,
        peer: PeerId,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) -> Result<()> {
        (**self).pull_chunk_v2(peer, cid, offset, max_bytes).await
    }
    async fn pull_manifest_v2(&self, peer: PeerId, merkle_root: [u8; 32]) -> Result<()> {
        (**self).pull_manifest_v2(peer, merkle_root).await
    }
}

/// What one waiter is waiting for: the peer and the request as this node
/// built it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Waiter {
    Chunk(PeerId, String),
    Manifest(PeerId, [u8; 32]),
}

impl Waiter {
    fn for_origin(origin: &OutboundOrigin) -> Option<Self> {
        match origin.kind() {
            OutboundRequestKind::V2Pull { cid } => Some(Waiter::Chunk(origin.peer(), cid.clone())),
            OutboundRequestKind::V2ManifestPull { merkle_root } => {
                Some(Waiter::Manifest(origin.peer(), *merkle_root))
            }
            _ => None,
        }
    }
}

type Answer = std::result::Result<ShardResponseV2, String>;

/// Outbound pulls for the re-fetch, settled by the listen loop.
pub struct ListenRefetchTransport<P> {
    sender: P,
    peers: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    pending: StdMutex<HashMap<Waiter, oneshot::Sender<Answer>>>,
    timeout: Duration,
}

impl<P: V2PullSender> ListenRefetchTransport<P> {
    /// `peers` is the listen loop's `PeerId → L1 address` map.
    pub fn new(
        sender: P,
        peers: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
        timeout: Duration,
    ) -> Self {
        Self {
            sender,
            peers,
            pending: StdMutex::new(HashMap::new()),
            timeout,
        }
    }

    /// Settle the waiter this event answers. `true` when it answered a
    /// re-fetch pull; `false` for every other event, which the listen loop
    /// handles as before.
    pub fn on_event(&self, event: &SumNetEvent) -> bool {
        let (origin, answer) = match event {
            SumNetEvent::ShardReceivedV2 {
                response, origin, ..
            } => (origin, Ok(response.clone())),
            SumNetEvent::ShardRequestFailed { error, origin, .. } => (origin, Err(error.clone())),
            _ => return false,
        };
        let Some(key) = Waiter::for_origin(origin) else {
            return false;
        };
        let waiter = self
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(&key);
        match waiter {
            Some(tx) => {
                let _ = tx.send(answer);
                true
            }
            None => false,
        }
    }

    /// Number of pulls waiting for an answer.
    pub fn in_flight(&self) -> usize {
        self.pending.lock().expect("pending mutex poisoned").len()
    }

    /// The connected, identified peer for `address`. When several peers carry
    /// the same identity, the lowest `PeerId` is used, so the choice is stable.
    async fn resolve(&self, address: [u8; 20]) -> Result<PeerId> {
        self.peers
            .read()
            .await
            .iter()
            .filter(|(_, a)| **a == address)
            .map(|(p, _)| *p)
            .min()
            .ok_or_else(|| {
                anyhow!(
                    "archive 0x{} is not connected and identified",
                    hex::encode(address)
                )
            })
    }

    async fn request<F>(&self, key: Waiter, send: F) -> Result<ShardResponseV2>
    where
        F: Future<Output = Result<()>> + Send,
    {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending mutex poisoned");
            if pending.contains_key(&key) {
                bail!("the same pull to the same archive is already in flight");
            }
            pending.insert(key.clone(), tx);
        }
        if let Err(e) = send.await {
            self.forget(&key);
            return Err(e);
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(error))) => Err(anyhow!("pull failed: {error}")),
            Ok(Err(_)) => Err(anyhow!("pull abandoned")),
            Err(_) => {
                self.forget(&key);
                Err(anyhow!("no answer within {}s", self.timeout.as_secs()))
            }
        }
    }

    fn forget(&self, key: &Waiter) {
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(key);
    }
}

#[async_trait]
impl<P: V2PullSender> RefetchTransport for ListenRefetchTransport<P> {
    async fn pull_manifest(&self, from: [u8; 20], merkle_root: [u8; 32]) -> Result<Vec<u8>> {
        let peer = self.resolve(from).await?;
        let response = self
            .request(
                Waiter::Manifest(peer, merkle_root),
                self.sender.pull_manifest_v2(peer, merkle_root),
            )
            .await?;
        match response {
            ShardResponseV2::ManifestData {
                manifest_bytes,
                error: None,
                ..
            } => Ok(manifest_bytes),
            ShardResponseV2::ManifestData { error: Some(e), .. } => {
                Err(anyhow!("manifest refused: {e}"))
            }
            _ => Err(anyhow!(
                "a manifest pull was answered with another response type"
            )),
        }
    }

    async fn pull_chunk(&self, from: [u8; 20], cid: &str, size: u64) -> Result<Vec<u8>> {
        let peer = self.resolve(from).await?;
        let response = self
            .request(
                Waiter::Chunk(peer, cid.to_string()),
                self.sender.pull_chunk_v2(peer, cid.to_string(), 0, size),
            )
            .await?;
        match response {
            ShardResponseV2::Data { error: Some(e), .. } => Err(anyhow!("chunk refused: {e}")),
            ShardResponseV2::Data {
                offset: 0,
                total_bytes,
                data,
                error: None,
                ..
            } if data.len() as u64 == total_bytes => Ok(data),
            ShardResponseV2::Data { .. } => Err(anyhow!("a partial or offset chunk answer")),
            _ => Err(anyhow!(
                "a chunk pull was answered with another response type"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        sent: StdMutex<Vec<String>>,
        fail_send: bool,
    }

    #[async_trait]
    impl V2PullSender for Recorder {
        async fn pull_chunk_v2(
            &self,
            peer: PeerId,
            cid: String,
            offset: u64,
            max_bytes: u64,
        ) -> Result<()> {
            if self.fail_send {
                bail!("swarm task has stopped");
            }
            self.sent
                .lock()
                .unwrap()
                .push(format!("chunk {peer} {cid} {offset} {max_bytes}"));
            Ok(())
        }
        async fn pull_manifest_v2(&self, peer: PeerId, root: [u8; 32]) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push(format!("manifest {peer} {}", hex::encode(root)));
            Ok(())
        }
    }

    const ARCHIVE: [u8; 20] = [0xA1; 20];

    fn transport(
        recorder: Recorder,
        timeout: Duration,
    ) -> (Arc<ListenRefetchTransport<Recorder>>, PeerId) {
        let peer = PeerId::random();
        let peers = Arc::new(RwLock::new(HashMap::from([(peer, ARCHIVE)])));
        (
            Arc::new(ListenRefetchTransport::new(recorder, peers, timeout)),
            peer,
        )
    }

    fn data(
        cid: &str,
        offset: u64,
        total: u64,
        bytes: &[u8],
        error: Option<&str>,
    ) -> ShardResponseV2 {
        ShardResponseV2::Data {
            cid: cid.into(),
            offset,
            total_bytes: total,
            data: bytes.to_vec(),
            error: error.map(Into::into),
        }
    }

    fn received(peer: PeerId, kind: OutboundRequestKind, response: ShardResponseV2) -> SumNetEvent {
        SumNetEvent::ShardReceivedV2 {
            peer_id: peer,
            response,
            origin: OutboundOrigin::new(peer, kind),
        }
    }

    /// Answer the first waiter with `event_for(peer)` once it is registered.
    fn answer_when_waiting(
        t: &Arc<ListenRefetchTransport<Recorder>>,
        event: SumNetEvent,
    ) -> tokio::task::JoinHandle<bool> {
        let t = t.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                if t.in_flight() > 0 {
                    return t.on_event(&event);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        })
    }

    #[tokio::test]
    async fn a_chunk_pull_is_settled_by_its_correlated_answer() {
        let (t, peer) = transport(Recorder::default(), Duration::from_secs(5));
        let kind = OutboundRequestKind::V2Pull { cid: "c1".into() };
        let answer = answer_when_waiting(&t, received(peer, kind, data("c1", 0, 3, b"abc", None)));
        let got = t.pull_chunk(ARCHIVE, "c1", 3).await.unwrap();
        assert_eq!(got, b"abc");
        assert!(answer.await.unwrap(), "the event settled the waiter");
        assert_eq!(
            *t.sender.sent.lock().unwrap(),
            vec![format!("chunk {peer} c1 0 3")]
        );
        assert_eq!(t.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_manifest_pull_is_settled_by_its_correlated_answer() {
        let (t, peer) = transport(Recorder::default(), Duration::from_secs(5));
        let root = [7u8; 32];
        let response = ShardResponseV2::ManifestData {
            merkle_root: root,
            manifest_bytes: b"cbor".to_vec(),
            error: None,
        };
        let answer = answer_when_waiting(
            &t,
            received(
                peer,
                OutboundRequestKind::V2ManifestPull { merkle_root: root },
                response,
            ),
        );
        assert_eq!(t.pull_manifest(ARCHIVE, root).await.unwrap(), b"cbor");
        assert!(answer.await.unwrap());
    }

    #[tokio::test]
    async fn refusals_partial_answers_and_failures_are_errors() {
        for (response, why) in [
            (data("c1", 0, 3, b"", Some("ACCESS_DENIED")), "refused"),
            (data("c1", 0, 9, b"abc", None), "partial"),
            (data("c1", 3, 6, b"abc", None), "offset"),
            (
                ShardResponseV2::ManifestData {
                    merkle_root: [0; 32],
                    manifest_bytes: vec![],
                    error: None,
                },
                "wrong variant",
            ),
        ] {
            let (t, peer) = transport(Recorder::default(), Duration::from_secs(5));
            let kind = OutboundRequestKind::V2Pull { cid: "c1".into() };
            let answer = answer_when_waiting(&t, received(peer, kind, response));
            assert!(t.pull_chunk(ARCHIVE, "c1", 3).await.is_err(), "{why}");
            assert!(answer.await.unwrap());
        }

        let (t, peer) = transport(Recorder::default(), Duration::from_secs(5));
        let failed = SumNetEvent::ShardRequestFailed {
            peer_id: peer,
            error: "connection closed".into(),
            origin: OutboundOrigin::new(peer, OutboundRequestKind::V2Pull { cid: "c1".into() }),
        };
        let answer = answer_when_waiting(&t, failed);
        let err = t
            .pull_chunk(ARCHIVE, "c1", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("connection closed"), "{err}");
        assert!(answer.await.unwrap());
    }

    #[tokio::test]
    async fn an_unidentified_archive_is_unreachable_and_nothing_is_sent() {
        let (t, _) = transport(Recorder::default(), Duration::from_secs(5));
        assert!(t.pull_chunk([0xEE; 20], "c1", 3).await.is_err());
        assert!(t.pull_manifest([0xEE; 20], [1; 32]).await.is_err());
        assert!(t.sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn silence_times_out_and_the_waiter_is_dropped() {
        let (t, _) = transport(Recorder::default(), Duration::from_millis(30));
        let err = t
            .pull_chunk(ARCHIVE, "c1", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no answer"), "{err}");
        assert_eq!(t.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_failed_send_leaves_no_waiter() {
        let (t, _) = transport(
            Recorder {
                fail_send: true,
                ..Recorder::default()
            },
            Duration::from_secs(5),
        );
        assert!(t.pull_chunk(ARCHIVE, "c1", 3).await.is_err());
        assert_eq!(t.in_flight(), 0);
    }

    #[tokio::test]
    async fn events_that_answer_no_waiter_are_left_to_the_listen_loop() {
        let (t, peer) = transport(Recorder::default(), Duration::from_secs(5));
        // Nothing in flight.
        let kind = OutboundRequestKind::V2Pull { cid: "c1".into() };
        assert!(!t.on_event(&received(peer, kind, data("c1", 0, 3, b"abc", None))));
        // A V1 request's answer, and an unrelated event.
        let v1 = SumNetEvent::ShardRequestFailed {
            peer_id: peer,
            error: "x".into(),
            origin: OutboundOrigin::new(peer, OutboundRequestKind::V1Chunk { cid: "c1".into() }),
        };
        assert!(!t.on_event(&v1));
        assert!(!t.on_event(&SumNetEvent::PeerConnected { peer_id: peer }));

        // An answer for another CID or another peer settles nothing.
        let t2 = t.clone();
        let pull = tokio::spawn(async move { t2.pull_chunk(ARCHIVE, "c1", 3).await });
        while t.in_flight() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let other_cid = OutboundRequestKind::V2Pull { cid: "c2".into() };
        assert!(!t.on_event(&received(peer, other_cid, data("c2", 0, 3, b"xyz", None))));
        let stranger = PeerId::random();
        let same_cid = OutboundRequestKind::V2Pull { cid: "c1".into() };
        assert!(!t.on_event(&received(
            stranger,
            same_cid.clone(),
            data("c1", 0, 3, b"xyz", None)
        )));
        assert!(t.on_event(&received(peer, same_cid, data("c1", 0, 3, b"abc", None))));
        assert_eq!(pull.await.unwrap().unwrap(), b"abc");
    }

    #[tokio::test]
    async fn the_same_pull_cannot_be_in_flight_twice() {
        let (t, _) = transport(Recorder::default(), Duration::from_millis(200));
        let t2 = t.clone();
        let first = tokio::spawn(async move { t2.pull_chunk(ARCHIVE, "c1", 3).await });
        while t.in_flight() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let err = t
            .pull_chunk(ARCHIVE, "c1", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already in flight"), "{err}");
        assert!(first.await.unwrap().is_err(), "the first one times out");
    }
}
