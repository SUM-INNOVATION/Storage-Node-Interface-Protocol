//! Which protocol a shard request actually negotiates, observed over a real
//! loopback QUIC connection.
//!
//! These were throwaway probes. They are permanent now, because the property
//! they check is the one WP-B turns on and there is no other way to see it.
//! `libp2p-request-response`'s `send_request` attaches **every** protocol its
//! behaviour was registered with to **every** request it sends (0.28.0,
//! `lib.rs`), and multistream-select then picks the first entry the remote also
//! supports. So the protocol a request ends up speaking is decided by the
//! behaviour's registration list and the remote's — never by the request. No
//! unit test on this side of the wire can observe the outcome; only a peer
//! can, and only by reporting what it negotiated.
//!
//! That is what the responder here does: its codec records the protocol name
//! libp2p hands to `read_request`, which is the negotiated protocol and nothing
//! else. Everything asserted below is that recorded name.
//!
//! ## What each case pins
//!
//! * A **V1 request to a dual-protocol peer negotiates `/sum/storage/v1`**.
//!   This is the case the old single dual-protocol behaviour got wrong: it
//!   offered `[v2, v1]` on every request, the dual peer picked v2, and the
//!   codec had to refuse to write a V1 payload onto a V2 stream. A V1 pull to
//!   a peer that also speaks V2 was inexpressible. Collapsing the two
//!   behaviours back into one is exactly what this case fails on.
//! * A **V1 request to a V1-only peer still works** — the legacy peers whose
//!   reachability the split exists to preserve.
//! * A **V2 request negotiates `/sum/storage/v2`**, against a dual peer and
//!   against a V2-only peer.
//! * A **V2 request to a V1-only peer fails**, and fails *without* falling back
//!   to V1. The absence of a fallback is the security property: a peer that
//!   declines the authenticated protocol must not be handed the
//!   unauthenticated one.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::prelude::*;
use libp2p::request_response::{self, Codec, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, SwarmBuilder};

use sum_net::{
    SHARD_XFER_PROTOCOL_V1, SHARD_XFER_PROTOCOL_V2, ShardRequest, ShardRequestV2,
    ShardRequestVersioned, ShardResponse, ShardResponseV2, ShardResponseVersioned,
    VersionedShardCodec, build_shard_xfer_v1, build_shard_xfer_v2, shard_xfer_config,
};

/// Whole-test budget. Loopback QUIC settles in milliseconds; this only bounds
/// a hang.
const CASE_TIMEOUT: Duration = Duration::from_secs(20);

// ── Recording codec ──────────────────────────────────────────────────────────

/// `VersionedShardCodec`, plus a note of every protocol name libp2p hands it.
///
/// The name passed to `read_request` *is* the negotiated protocol — libp2p
/// resolves multistream-select before it calls the codec — so recording it is
/// the whole measurement. The codec is otherwise the production one, so a
/// payload that the real codec would refuse is refused here too.
#[derive(Debug, Clone, Default)]
struct ProbeCodec {
    inner: VersionedShardCodec,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Codec for ProbeCodec {
    type Protocol = String;
    type Request = ShardRequestVersioned;
    type Response = ShardResponseVersioned;

    async fn read_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        self.seen.lock().unwrap().push(protocol.clone());
        self.inner.read_request(protocol, io).await
    }

    async fn read_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        self.inner.read_response(protocol, io).await
    }

    async fn write_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        self.inner.write_request(protocol, io, req).await
    }

    async fn write_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        self.inner.write_response(protocol, io, res).await
    }
}

// ── Behaviours ───────────────────────────────────────────────────────────────

/// The initiator's shard surface, built from the **production** constructors.
///
/// Using `build_shard_xfer_v1` / `build_shard_xfer_v2` rather than a local copy
/// is deliberate: these cases are only evidence about production if they send
/// through the registration lists production registers.
#[derive(NetworkBehaviour)]
struct Initiator {
    v1: request_response::Behaviour<VersionedShardCodec>,
    v2: request_response::Behaviour<VersionedShardCodec>,
}

/// Which peer the initiator is talking to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Remote {
    /// Speaks both protocols — the interesting case, because it is the one
    /// where a shared protocol list could pick the wrong entry.
    Dual,
    /// Speaks `/sum/storage/v1` only. A legacy node.
    V1Only,
    /// Speaks `/sum/storage/v2` only.
    V2Only,
}

impl Remote {
    fn protocols(self) -> Vec<(String, ProtocolSupport)> {
        match self {
            // V2 first, as a dual peer that preferred V2 would list it. The
            // point of the split is that the initiator's list has one entry, so
            // the remote's preference cannot pull a V1 request onto V2.
            Self::Dual => vec![
                (SHARD_XFER_PROTOCOL_V2.to_string(), ProtocolSupport::Full),
                (SHARD_XFER_PROTOCOL_V1.to_string(), ProtocolSupport::Full),
            ],
            Self::V1Only => vec![(SHARD_XFER_PROTOCOL_V1.to_string(), ProtocolSupport::Full)],
            Self::V2Only => vec![(SHARD_XFER_PROTOCOL_V2.to_string(), ProtocolSupport::Full)],
        }
    }
}

/// Which protocol version the initiator asks on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    V1Pull,
    V2Pull,
}

/// What a case observed.
#[derive(Debug)]
struct Outcome {
    /// Protocol names the responder's codec was handed, in arrival order.
    negotiated: Vec<String>,
    /// Whether the initiator received a response.
    answered: bool,
    /// The outbound failure, if the request never got that far.
    failure: Option<String>,
}

impl Outcome {
    fn sole_protocol(&self) -> &str {
        assert_eq!(
            self.negotiated.len(),
            1,
            "expected exactly one negotiated stream, saw {:?} (failure: {:?})",
            self.negotiated,
            self.failure
        );
        &self.negotiated[0]
    }
}

// ── Harness ──────────────────────────────────────────────────────────────────

fn quic_swarm<B: NetworkBehaviour>(behaviour: impl FnOnce() -> B) -> Swarm<B> {
    SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_quic()
        .with_behaviour(|_| behaviour())
        .expect("behaviour construction is infallible")
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(30)))
        .build()
}

/// Answer a request with a well-formed response of the same version, so the
/// initiator's side of the exchange completes rather than timing out.
fn echo(request: &ShardRequestVersioned) -> ShardResponseVersioned {
    match request {
        ShardRequestVersioned::V1(req) => ShardResponseVersioned::V1(ShardResponse {
            cid: req.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: None,
        }),
        ShardRequestVersioned::V2(ShardRequestV2::Pull { cid, offset, .. }) => {
            ShardResponseVersioned::V2(ShardResponseV2::Data {
                cid: cid.clone(),
                offset: *offset,
                total_bytes: 0,
                data: Vec::new(),
                error: None,
            })
        }
        ShardRequestVersioned::V2(other) => {
            panic!("this harness only sends V2 pulls, got {other:?}")
        }
    }
}

async fn run_case(remote: Remote, send: Ask) -> Outcome {
    tokio::time::timeout(CASE_TIMEOUT, run_case_inner(remote, send))
        .await
        .unwrap_or_else(|_| panic!("case {remote:?}/{send:?} did not settle in {CASE_TIMEOUT:?}"))
}

async fn run_case_inner(remote: Remote, send: Ask) -> Outcome {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();

    let mut responder = {
        let codec = ProbeCodec {
            inner: VersionedShardCodec::default(),
            seen: Arc::clone(&seen),
        };
        let protocols = remote.protocols();
        quic_swarm(move || {
            request_response::Behaviour::with_codec(codec, protocols, shard_xfer_config())
        })
    };
    let responder_id: PeerId = *responder.local_peer_id();

    responder
        .listen_on("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .expect("loopback QUIC listener");

    // Wait for the listener's concrete address before dialing it.
    let listen_addr: Multiaddr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = responder.select_next_some().await {
            break address;
        }
    };

    let mut initiator = quic_swarm(|| Initiator {
        v1: build_shard_xfer_v1(),
        v2: build_shard_xfer_v2(),
    });
    initiator.dial(listen_addr).expect("dial the responder");

    let mut connected = false;
    let mut sent = false;
    let mut answered = false;
    let mut failure: Option<String> = None;

    while failure.is_none() && !answered {
        tokio::select! {
            ev = initiator.select_next_some() => match ev {
                SwarmEvent::ConnectionEstablished { .. } if !connected => {
                    connected = true;
                }
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    failure = Some(format!("dial failed: {error}"));
                }
                SwarmEvent::Behaviour(InitiatorEvent::V1(e))
                | SwarmEvent::Behaviour(InitiatorEvent::V2(e)) => match e {
                    request_response::Event::Message {
                        message: request_response::Message::Response { .. },
                        ..
                    } => answered = true,
                    request_response::Event::OutboundFailure { error, .. } => {
                        failure = Some(error.to_string());
                    }
                    _ => {}
                },
                _ => {}
            },
            ev = responder.select_next_some() => {
                if let SwarmEvent::Behaviour(request_response::Event::Message {
                    message: request_response::Message::Request { request, channel, .. },
                    ..
                }) = ev
                {
                    let response = echo(&request);
                    let _ = responder
                        .behaviour_mut()
                        .send_response(channel, response);
                }
            },
        }

        // Send once the connection is up, so `send_request` is not queued
        // behind a dial and the negotiation under test is the real one.
        if connected && !sent {
            sent = true;
            match send {
                Ask::V1Pull => {
                    initiator.behaviour_mut().v1.send_request(
                        &responder_id,
                        ShardRequestVersioned::V1(ShardRequest {
                            cid: "probe-cid".into(),
                            offset: None,
                            max_bytes: None,
                            push_data: None,
                        }),
                    );
                }
                Ask::V2Pull => {
                    initiator.behaviour_mut().v2.send_request(
                        &responder_id,
                        ShardRequestVersioned::V2(ShardRequestV2::Pull {
                            cid: "probe-cid".into(),
                            offset: 0,
                            max_bytes: 16,
                        }),
                    );
                }
            }
        }
    }

    let negotiated = seen.lock().unwrap().clone();
    Outcome {
        negotiated,
        answered,
        failure,
    }
}

// ── Cases ────────────────────────────────────────────────────────────────────

/// The case the split exists for.
///
/// One behaviour carrying `[v2, v1]` offers both on every request; a dual peer
/// picks v2; a V1 payload cannot be written to a V2 stream. So under the old
/// arrangement a V1 pull to a V2-capable peer could not be made at all. Here
/// the V1 behaviour offers `/sum/storage/v1` alone and there is nothing else to
/// pick.
#[tokio::test]
async fn a_v1_request_to_a_dual_peer_negotiates_v1() {
    let out = run_case(Remote::Dual, Ask::V1Pull).await;
    assert_eq!(
        out.sole_protocol(),
        SHARD_XFER_PROTOCOL_V1,
        "a V1 request must negotiate V1 even against a peer that prefers V2 \
         (failure: {:?})",
        out.failure
    );
    assert!(
        !out.negotiated.iter().any(|p| p == SHARD_XFER_PROTOCOL_V2),
        "a V1 request must not open a V2 stream"
    );
    assert!(out.answered, "and it must complete: {:?}", out.failure);
}

/// The legacy peers the V1 pull path exists to keep reachable.
#[tokio::test]
async fn a_v1_request_to_a_v1_only_peer_negotiates_v1() {
    let out = run_case(Remote::V1Only, Ask::V1Pull).await;
    assert_eq!(out.sole_protocol(), SHARD_XFER_PROTOCOL_V1);
    assert!(out.answered, "V1 pull must keep working: {:?}", out.failure);
}

#[tokio::test]
async fn a_v2_request_to_a_dual_peer_negotiates_v2() {
    let out = run_case(Remote::Dual, Ask::V2Pull).await;
    assert_eq!(
        out.sole_protocol(),
        SHARD_XFER_PROTOCOL_V2,
        "failure: {:?}",
        out.failure
    );
    assert!(out.answered, "{:?}", out.failure);
}

#[tokio::test]
async fn a_v2_request_to_a_v2_only_peer_negotiates_v2() {
    let out = run_case(Remote::V2Only, Ask::V2Pull).await;
    assert_eq!(out.sole_protocol(), SHARD_XFER_PROTOCOL_V2);
    assert!(out.answered, "{:?}", out.failure);
}

/// No fallback, and the shape of the non-fallback matters: the request fails,
/// and the V1-only peer's codec is never handed a stream at all. A "graceful
/// degradation" that quietly retried on V1 would show up here as a
/// `/sum/storage/v1` entry, and would mean an unauthenticated write path
/// reachable by declining the authenticated one.
#[tokio::test]
async fn a_v2_request_to_a_v1_only_peer_fails_without_falling_back_to_v1() {
    let out = run_case(Remote::V1Only, Ask::V2Pull).await;
    assert!(
        out.failure.is_some(),
        "a V2 request to a V1-only peer must fail, not succeed"
    );
    assert!(!out.answered);
    assert!(
        out.negotiated.is_empty(),
        "no stream may be opened to a peer that does not speak V2 — saw {:?}",
        out.negotiated
    );
}
