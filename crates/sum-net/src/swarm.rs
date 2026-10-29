use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, Transport,
    core::{
        muxing::StreamMuxerBox,
        transport::{Boxed, timeout::TransportTimeout},
        upgrade::Version,
    },
    dcutr, gossipsub, identify,
    identity::Keypair,
    kad, mdns,
    multiaddr::Protocol,
    relay,
    request_response::{self, ResponseChannel},
    swarm::{SwarmEvent, behaviour::toggle::Toggle, dial_opts::DialOpts},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use sum_types::config::NetConfig;

use crate::{
    behaviour::{
        LocalMeshBehaviour, LocalMeshBehaviourEvent, build_shard_xfer_v1, build_shard_xfer_v2,
    },
    codec::{
        ShardRequest, ShardRequestV2, ShardRequestVersioned, ShardResponse, ShardResponseV2,
        ShardResponseVersioned,
    },
    correlation::{
        FailureOutcome, OUTBOUND_RECORD_TTL, OutboundKey, OutboundOrigin, OutboundRequestKind,
        OutboundTracker, RequestDomain,
    },
    discovery,
    events::SumNetEvent,
    gate::{Gated, PolicyBehaviour, PolicyTransport},
    gossip::GossipManager,
    nat,
    policy::{ConnectionAudit, NetPolicy, PolicyRefusals, PolicySource},
};

/// Transport connection timeout. The value `SwarmBuilder` applies by default,
/// kept so the hand-composed stack below behaves as the builder-made one did.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle-connection timeout, as before.
const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a pending response channel is kept before it is considered orphaned.
/// Matches the request-response timeout (120s) so the requester has already
/// given up by the time we reap.
const PENDING_CHANNEL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the reaper runs to clean up orphaned pending channels.
const REAPER_INTERVAL: Duration = Duration::from_secs(30);

// ── SwarmCommand ──────────────────────────────────────────────────────────────

/// Commands sent from the [`crate::SumNet`] handle into the running swarm loop.
///
/// V1 and V2 outbound paths each get their own command variant **and their own
/// behaviour**. There is no shared behaviour and no negotiation between the
/// versions: [`SwarmCommand::RequestShard`] is routed to
/// `LocalMeshBehaviour::shard_xfer_v1`, which advertises `/sum/storage/v1` and
/// nothing else, and every V2 command is routed to `shard_xfer_v2`, which
/// advertises `/sum/storage/v2` and nothing else. See [`crate::behaviour`].
///
/// There is deliberately **no outbound V1 push command**. V1 push was
/// unauthenticated and unauthorized; it is retired, and its absence here is
/// what makes "no V1-push fallback" a property of the type rather than of a
/// convention. Pushes go out as [`SwarmCommand::RequestShardV2`] carrying
/// `ShardRequestV2::Push` or `ShardRequestV2::ManifestPush`.
#[derive(Debug)]
pub enum SwarmCommand {
    /// Publish bytes to a named Gossipsub topic.
    Publish { topic: String, data: Vec<u8> },

    /// Send a V1 **pull** to a remote peer, on the V1-only behaviour.
    ///
    /// This is the path that keeps V1-only peers reachable: the request
    /// carries `/sum/storage/v1` alone, so a peer that speaks only V1
    /// negotiates it, and a peer that speaks both does too.
    RequestShard {
        peer_id: PeerId,
        request: ShardRequest,
    },

    /// Send a V2 request (Pull / Push / ManifestPush / ManifestPull) to
    /// a remote peer. Carries the chain-plan-v3.2 request shape directly.
    RequestShardV2 {
        peer_id: PeerId,
        request: ShardRequestV2,
    },

    /// Send a V1 response on a channel that arrived on the **V1** behaviour.
    ///
    /// Both behaviours yield `ResponseChannel<ShardResponseVersioned>`, the
    /// same Rust type, so the type system cannot tell them apart. The pending
    /// map records which behaviour each channel came from and this command is
    /// refused on a V2 channel — see [`take_channel`].
    SendShardResponse {
        channel_id: u64,
        response: ShardResponse,
    },

    /// Send a V2 response on a channel that arrived on the **V2** behaviour.
    /// Refused on a V1 channel, for the reason above.
    SendShardResponseV2 {
        channel_id: u64,
        response: ShardResponseV2,
    },

    /// Exit the event loop cleanly.
    Shutdown,
}

// ── Response-channel provenance ──────────────────────────────────────────────

/// A response channel, together with the behaviour it arrived on.
///
/// `request_response::Behaviour<VersionedShardCodec>` is instantiated twice —
/// once per protocol — so both behaviours hand back the *same Rust type*,
/// `ResponseChannel<ShardResponseVersioned>`. Nothing in that type records
/// which behaviour minted it, and handing a channel back to the wrong
/// behaviour is not a compile error: `send_response` takes it, fails to find
/// the matching inbound request in its own table, and returns the response as
/// an `Err` that reads exactly like a closed connection. The peer waits out the
/// 120s timeout.
///
/// So provenance is recorded here, at the one point where it is still known,
/// and checked at every respond path.
///
/// Generic over the channel type only so the domain check can be unit-tested
/// without a live swarm; production always uses
/// `PendingChannel<ResponseChannel<ShardResponseVersioned>>`.
#[derive(Debug)]
pub(crate) struct PendingChannel<C> {
    /// The behaviour this channel arrived on. The respond path must match.
    pub(crate) domain: RequestDomain,
    pub(crate) channel: C,
}

/// The outcome of asking for a pending channel in a particular domain.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChannelTake<C> {
    /// The channel was filed under the requested domain and is now yours.
    Taken(C),
    /// A channel exists under this id, but it came from the other behaviour.
    /// It is **left in place** — the domain that owns it may still answer on
    /// it, and the reaper will collect it if nobody does.
    WrongDomain { have: RequestDomain },
    /// No channel under this id: already answered, or already reaped.
    Missing,
}

/// Take the channel filed under `channel_id`, but only if it came from the
/// behaviour named by `want`.
///
/// The mismatch case does not remove the entry. Removing it would convert a
/// caller's routing bug into a silently dropped inbound request; leaving it
/// means the correct responder can still answer, and the refusal is counted.
pub(crate) fn take_channel<C>(
    map: &mut HashMap<u64, (PendingChannel<C>, Instant)>,
    channel_id: u64,
    want: RequestDomain,
) -> ChannelTake<C> {
    match map.get(&channel_id) {
        None => ChannelTake::Missing,
        Some((pending, _)) if pending.domain != want => ChannelTake::WrongDomain {
            have: pending.domain,
        },
        Some(_) => {
            let (pending, _) = map.remove(&channel_id).expect("checked immediately above");
            ChannelTake::Taken(pending.channel)
        }
    }
}

// ── Refusal telemetry ────────────────────────────────────────────────────────

/// Counts of refusals the swarm made, as **fixed struct fields**.
///
/// Deliberately not a map keyed by anything. Every field below is one line of
/// code in this file; a labelled counter would let a remote peer, a CID, or a
/// protocol string it chose become a metric key, and an attacker who can name
/// the key can grow the metric set without bound. Fixed fields make the
/// cardinality of this telemetry a property of the source.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SwarmRefusals {
    /// A V1 response was offered for a channel that arrived on the V2
    /// behaviour.
    pub v1_response_on_v2_channel: u64,
    /// A V2 response was offered for a channel that arrived on the V1
    /// behaviour.
    pub v2_response_on_v1_channel: u64,
    /// A response was offered for a channel id with no pending channel.
    pub response_without_channel: u64,
    /// An outbound request's recorded kind disagreed with the domain it was
    /// filed under — a routing bug; the request is left unrecorded.
    pub outbound_domain_protocol_mismatch: u64,
    /// Two outbound requests claimed one key; both were abandoned.
    pub outbound_id_collision: u64,
    /// An inbound request whose payload version did not match the protocol
    /// its behaviour speaks. Structurally impossible; counted so that
    /// "impossible" stays observable.
    pub inbound_version_domain_mismatch: u64,
}

// ── SumSwarm ─────────────────────────────────────────────────────────────────

/// Owns the [`libp2p::Swarm`] and the [`GossipManager`].
/// Constructed by [`SumSwarm::build`] and consumed by [`SumSwarm::run`].
pub struct SumSwarm {
    inner: libp2p::Swarm<LocalMeshBehaviour>,
    gossip: GossipManager,

    /// Response channels from inbound shard requests, each tagged with the
    /// behaviour it arrived on and the time it was filed.
    ///
    /// One map for both behaviours, because a channel id is allocated here and
    /// is unique across both. The [`RequestDomain`] in each entry is what the
    /// shared Rust type cannot carry — see [`PendingChannel`].
    pending_shard_channels: HashMap<
        u64,
        (
            PendingChannel<ResponseChannel<ShardResponseVersioned>>,
            Instant,
        ),
    >,

    /// Monotonic counter for channel IDs.
    next_channel_id: u64,

    /// Identity of every outbound chunk request still awaiting a terminal
    /// event, recorded from the request this node built. A response is only
    /// surfaced to the upper layer if it matches the record filed under its
    /// request id — see [`crate::correlation`]. Keyed by
    /// `(RequestDomain, OutboundRequestId)` because the two shard behaviours
    /// mint colliding raw ids.
    outbound: OutboundTracker,

    /// Fixed-cardinality refusal counters. See [`SwarmRefusals`].
    refusals: SwarmRefusals,

    /// Candidate relay peers indexed by peer id. Seeded from
    /// `--bootstrap-peer` (with `confirmed: false`) and promoted to
    /// `confirmed: true` when identify reports that the peer advertises the
    /// relay hop protocol. Only confirmed candidates are reservation targets.
    relay_peers: HashMap<PeerId, nat::RelayCandidate>,

    /// Current NAT status as determined by AutoNAT.
    nat_status: nat::NatStatus,

    /// Tri-state reservation machine (see [`nat::RelayReservationState`]):
    /// `None` → `Pending(peer)` on listen_on success → `Active(peer)` on
    /// `ReservationReqAccepted` → `None` again on `ListenerClosed` (denial,
    /// timeout, explicit close). Prevents reservation stacking and wedging.
    active_relay_reservation: nat::RelayReservationState,

    /// Per-peer reference count of currently-open **non-relayed** (direct)
    /// connections. Used by the direct-dial shortcut to avoid stacking
    /// redundant dials when we already have a usable direct path. Updated
    /// from `SwarmEvent::ConnectionEstablished` / `ConnectionClosed`.
    direct_connections: HashMap<PeerId, u32>,

    /// Network policy (SNIP #79), shared with the transport gate and the
    /// policy behaviours. Unrestricted unless `--outbound-allow` is set.
    policy: Arc<NetPolicy>,

    /// Refusal totals at the last periodic summary, so the summary is logged
    /// only when something changed.
    last_refusals: PolicyRefusals,
}

impl SumSwarm {
    /// Construct and configure the Swarm with an externally-provided keypair.
    ///
    /// The keypair should be derived from the user's SUM Chain L1 wallet seed
    /// via [`crate::identity::keypair_from_seed`].
    ///
    /// Transport:  QUIC always; TCP+Noise+Yamux added when `enable_wan` is true.
    /// Behaviour:  mDNS (unless disabled) + Gossipsub + Identify + chunk
    ///             transfer + Kademlia DHT.
    /// Listener:   `0.0.0.0:<listen_port>` (QUIC) + `0.0.0.0:<tcp_listen_port>`
    ///             (TCP, if WAN); `0.0.0.0` is replaced by `config.listen_ip`
    ///             when set.
    ///
    /// Fails if `config.outbound_allow` holds a malformed entry. The effective
    /// network policy is logged once here.
    pub fn build(config: &NetConfig, keypair: Keypair) -> Result<Self> {
        let policy = Arc::new(NetPolicy::from_config(config)?);
        policy.log_effective();

        let mut swarm = build_swarm(config, keypair, policy.clone())?;

        // QUIC listener (always), TCP listener (WAN mode only). The network
        // policy does not change listeners.
        for addr in listen_addrs(config)? {
            let what = if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                "failed to bind QUIC listener"
            } else {
                "failed to bind TCP listener"
            };
            swarm.listen_on(addr).context(what)?;
        }

        Ok(Self {
            inner: swarm,
            gossip: GossipManager::new(),
            pending_shard_channels: HashMap::new(),
            next_channel_id: 0,
            outbound: OutboundTracker::default(),
            refusals: SwarmRefusals::default(),
            relay_peers: HashMap::new(),
            nat_status: nat::NatStatus::Unknown,
            active_relay_reservation: nat::RelayReservationState::None,
            direct_connections: HashMap::new(),
            policy,
            last_refusals: PolicyRefusals::default(),
        })
    }

    /// The network policy this swarm runs under.
    pub fn policy(&self) -> Arc<NetPolicy> {
        self.policy.clone()
    }

    /// Bootstrap Kademlia DHT with the provided peer multiaddrs.
    ///
    /// Each address must end with `/p2p/<peer_id>`. The node dials the
    /// bootstrap peers and initiates a Kademlia bootstrap query.
    ///
    /// Under an outbound allow-list a bootstrap address that is not allowed
    /// is refused (logged and counted under `bootstrap`) and neither stored
    /// nor dialed. If every bootstrap address is refused the Kademlia
    /// bootstrap query is skipped with a warning instead of failing startup.
    pub fn bootstrap_kademlia(&mut self, bootstrap_peers: &[String]) -> Result<()> {
        let mut admitted = 0usize;
        for addr_str in bootstrap_peers {
            let addr: Multiaddr = addr_str
                .parse()
                .context(format!("invalid bootstrap multiaddr: {addr_str}"))?;

            // Extract PeerId from the last /p2p/<peer_id> component.
            let peer_id = addr
                .iter()
                .find_map(|proto| {
                    if let libp2p::multiaddr::Protocol::P2p(pid) = proto {
                        Some(pid)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("bootstrap addr missing /p2p/ component: {addr_str}")
                })?;

            if !self
                .policy
                .admit(PolicySource::Bootstrap, &addr, Some(peer_id))
            {
                continue;
            }
            admitted += 1;

            self.inner
                .behaviour_mut()
                .kademlia
                .add_address(&peer_id, addr.clone());

            // Stash the bootstrap address as an UNCONFIRMED relay candidate.
            // We explicitly do NOT set `confirmed = true` — that flag flips
            // only when identify reports the remote advertises the relay
            // hop protocol. Unconfirmed entries are ignored by the AutoNAT
            // reservation path, so passing `--bootstrap-peer` for a peer
            // that isn't a relay won't cause us to reserve against it.
            let entry = self.relay_peers.entry(peer_id).or_default();
            if is_dialable_over_wan(&addr) && !entry.addrs.contains(&addr) {
                entry.addrs.push(addr.clone());
            }

            info!(%peer_id, %addr, "added Kademlia bootstrap peer");

            let opts = DialOpts::from(addr);
            self.policy
                .label_dial(opts.connection_id(), PolicySource::Bootstrap);
            if let Err(e) = self.inner.dial(opts) {
                warn!(%peer_id, %e, "failed to dial bootstrap peer");
            }
        }

        if !bootstrap_peers.is_empty() && admitted == 0 {
            warn!(
                configured = bootstrap_peers.len(),
                "network policy: no bootstrap peer is allowed by --outbound-allow — \
                 Kademlia bootstrap skipped"
            );
            return Ok(());
        }

        if !bootstrap_peers.is_empty() {
            self.inner
                .behaviour_mut()
                .kademlia
                .bootstrap()
                .map_err(|e| anyhow::anyhow!("Kademlia bootstrap failed: {e}"))?;
            info!(
                peers = bootstrap_peers.len(),
                "Kademlia bootstrap initiated"
            );
        }

        Ok(())
    }

    /// Subscribe the node to all SUM Storage Node Gossipsub topics.
    pub fn subscribe_all_topics(&mut self) -> Result<()> {
        self.gossip
            .subscribe_all(&mut self.inner.behaviour_mut().gossipsub)
    }

    /// Publish bytes to a named topic from within the event loop.
    pub fn publish(&mut self, topic: &str, data: Vec<u8>) -> Result<()> {
        self.gossip
            .publish(&mut self.inner.behaviour_mut().gossipsub, topic, data)
            .map(|_| ())
    }

    /// The core async event loop.
    pub async fn run(
        mut self,
        event_tx: mpsc::Sender<SumNetEvent>,
        mut cmd_rx: mpsc::Receiver<SwarmCommand>,
    ) -> Result<()> {
        let mut reaper_interval = tokio::time::interval(REAPER_INTERVAL);

        loop {
            tokio::select! {
                event = self.inner.select_next_some() => {
                    self.handle_swarm_event(event, &event_tx);
                }

                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SwarmCommand::Publish { topic, data }) => {
                            if let Err(e) = self.publish(&topic, data) {
                                warn!(%e, %topic, "gossipsub publish failed");
                            }
                        }
                        Some(SwarmCommand::RequestShard { peer_id, request }) => {
                            // Explicit V1 routing. The V1 behaviour advertises
                            // `/sum/storage/v1` alone, so this negotiates V1
                            // against a V1-only peer and against a dual peer
                            // alike — which is what keeps V1 pull working now
                            // that V2 is no longer preferred on a shared list.
                            //
                            // Classify before the request is moved into libp2p:
                            // this is the last point at which the locally-built
                            // request is in hand.
                            let kind = OutboundRequestKind::from_v1(&request);
                            let id = self.inner.behaviour_mut().shard_xfer_v1
                                .send_request(&peer_id, ShardRequestVersioned::V1(request));
                            self.record_outbound(RequestDomain::ShardXferV1, id, peer_id, kind);
                        }
                        Some(SwarmCommand::RequestShardV2 { peer_id, request }) => {
                            // Explicit V2 routing for every V2 operation —
                            // Pull, Push, ManifestPush, ManifestPull all leave
                            // on the V2-only behaviour.
                            let kind = OutboundRequestKind::from_v2(&request);
                            let id = self.inner.behaviour_mut().shard_xfer_v2
                                .send_request(&peer_id, ShardRequestVersioned::V2(request));
                            self.record_outbound(RequestDomain::ShardXferV2, id, peer_id, kind);
                        }
                        Some(SwarmCommand::SendShardResponse { channel_id, response }) => {
                            let cid_for_log = response.cid.clone();
                            match take_channel(
                                &mut self.pending_shard_channels,
                                channel_id,
                                RequestDomain::ShardXferV1,
                            ) {
                                ChannelTake::Taken(channel) => {
                                    if let Err(resp) = self.inner.behaviour_mut().shard_xfer_v1
                                        .send_response(channel, ShardResponseVersioned::V1(response))
                                    {
                                        let resp_cid = match &resp {
                                            ShardResponseVersioned::V1(r) => r.cid.as_str(),
                                            ShardResponseVersioned::V2(_) => "<V2 response on V1 path?>",
                                        };
                                        warn!(cid = %cid_for_log, returned_cid = %resp_cid, "failed to send V1 chunk response — channel closed");
                                    }
                                }
                                ChannelTake::WrongDomain { have } => {
                                    self.refusals.v1_response_on_v2_channel += 1;
                                    error!(
                                        channel_id,
                                        cid = %cid_for_log,
                                        channel_domain = have.label(),
                                        "refused: V1 response offered for a channel that arrived on \
                                         the V2 behaviour — the channel is left for its own domain"
                                    );
                                }
                                ChannelTake::Missing => {
                                    self.refusals.response_without_channel += 1;
                                    warn!(channel_id, "no pending channel for V1 chunk response");
                                }
                            }
                        }
                        Some(SwarmCommand::SendShardResponseV2 { channel_id, response }) => {
                            match take_channel(
                                &mut self.pending_shard_channels,
                                channel_id,
                                RequestDomain::ShardXferV2,
                            ) {
                                ChannelTake::Taken(channel) => {
                                    if self.inner.behaviour_mut().shard_xfer_v2
                                        .send_response(channel, ShardResponseVersioned::V2(response)).is_err()
                                    {
                                        warn!(channel_id, "failed to send V2 response — channel closed");
                                    }
                                }
                                ChannelTake::WrongDomain { have } => {
                                    self.refusals.v2_response_on_v1_channel += 1;
                                    error!(
                                        channel_id,
                                        channel_domain = have.label(),
                                        "refused: V2 response offered for a channel that arrived on \
                                         the V1 behaviour — the channel is left for its own domain"
                                    );
                                }
                                ChannelTake::Missing => {
                                    self.refusals.response_without_channel += 1;
                                    warn!(channel_id, "no pending channel for V2 response");
                                }
                            }
                        }
                        Some(SwarmCommand::Shutdown) | None => {
                            self.log_policy_summary();
                            info!("swarm event loop shutting down");
                            return Ok(());
                        }
                    }
                }

                _ = reaper_interval.tick() => {
                    self.reap_orphaned_channels();
                    self.log_policy_summary();
                }
            }
        }
    }

    /// Remove pending response channels that have been waiting longer than
    /// [`PENDING_CHANNEL_TIMEOUT`]. Dropping the `ResponseChannel` causes
    /// libp2p to signal a timeout to the requester.
    fn reap_orphaned_channels(&mut self) {
        let reaped = reap_stale_entries(&mut self.pending_shard_channels, PENDING_CHANNEL_TIMEOUT);
        if reaped > 0 {
            info!(
                reaped,
                remaining = self.pending_shard_channels.len(),
                "orphaned channel cleanup"
            );
        }

        // Outbound correlation records are removed by the response and failure
        // paths, both of which libp2p guarantees to deliver. This is the
        // backstop: if either guarantee is ever broken, the pending set is
        // capped by TTL rather than growing for the life of the process.
        let stale = self.outbound.reap(Instant::now(), OUTBOUND_RECORD_TTL);
        if stale > 0 {
            warn!(
                reaped = stale,
                remaining = self.outbound.len(),
                "outbound request records expired without a terminal event"
            );
        }
    }

    /// Log the policy refusal totals if they changed since the last summary.
    /// Silent without an allow-list and while nothing has been refused.
    fn log_policy_summary(&mut self) {
        if !self.policy.is_restricted() {
            return;
        }
        let now = self.policy.refusals();
        if now == self.last_refusals {
            return;
        }
        self.last_refusals = now;
        info!(
            total = now.total(),
            bootstrap = now.bootstrap,
            mdns = now.mdns,
            identify = now.identify,
            kad = now.kad,
            autodial = now.autodial,
            inbound = now.inbound,
            other = now.other,
            "network policy: refusal totals"
        );
    }

    /// Snapshot of the fixed-field refusal counters.
    pub fn refusals(&self) -> SwarmRefusals {
        self.refusals
    }

    /// File an outbound request under the domain of the behaviour that sent it.
    ///
    /// One helper for both routes so the domain and the behaviour are chosen on
    /// adjacent lines at each call site, and so the two refusal counters are
    /// incremented in one place.
    fn record_outbound(
        &mut self,
        domain: RequestDomain,
        id: request_response::OutboundRequestId,
        peer_id: PeerId,
        kind: OutboundRequestKind,
    ) {
        let label = kind.label();
        if let Err(e) = self
            .outbound
            .record(OutboundKey::new(domain, id), peer_id, kind)
        {
            if e.is_domain_protocol_mismatch() {
                self.refusals.outbound_domain_protocol_mismatch += 1;
                error!(%peer_id, %e, asked = label, "outbound request routed to the wrong behaviour — not recorded");
            } else {
                self.refusals.outbound_id_collision += 1;
                error!(%peer_id, %e, "outbound request identity collision — key poisoned, both requests abandoned");
            }
        }
    }

    /// Handle one request-response event from either shard behaviour.
    ///
    /// `domain` names which behaviour it came from. Everything version-specific
    /// downstream — the outbound key, the channel provenance, the refusal
    /// counters — is derived from it rather than from anything on the wire.
    fn on_shard_event(
        &mut self,
        domain: RequestDomain,
        event: request_response::Event<ShardRequestVersioned, ShardResponseVersioned>,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => self.on_inbound_request(domain, peer, request, channel, event_tx),
                request_response::Message::Response {
                    request_id,
                    response,
                } => self.on_inbound_response(domain, peer, request_id, response, event_tx),
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => self.on_outbound_failure(domain, peer, request_id, &error, event_tx),
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!(%peer, %error, protocol = domain.protocol(), "chunk request inbound failure");
            }
            request_response::Event::ResponseSent { peer, .. } => {
                debug!(%peer, protocol = domain.protocol(), "chunk response sent");
            }
        }
    }

    fn on_inbound_request(
        &mut self,
        domain: RequestDomain,
        peer: PeerId,
        request: ShardRequestVersioned,
        channel: ResponseChannel<ShardResponseVersioned>,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        // The codec decodes on the negotiated protocol name and each behaviour
        // negotiates exactly one protocol, so the payload version and the
        // domain cannot disagree. Check anyway: if they ever do, the channel is
        // dropped here rather than filed under a domain that would refuse to
        // answer on it, which would leave the peer waiting out the timeout.
        let versions_agree = matches!(
            (domain, &request),
            (RequestDomain::ShardXferV1, ShardRequestVersioned::V1(_))
                | (RequestDomain::ShardXferV2, ShardRequestVersioned::V2(_))
        );
        if !versions_agree {
            self.refusals.inbound_version_domain_mismatch += 1;
            error!(
                %peer,
                behaviour = domain.label(),
                "inbound request version does not match the behaviour it arrived on — dropped"
            );
            drop(channel);
            return;
        }

        let channel_id = self.next_channel_id;
        self.next_channel_id += 1;
        self.pending_shard_channels.insert(
            channel_id,
            (PendingChannel { domain, channel }, Instant::now()),
        );

        match request {
            ShardRequestVersioned::V1(req) => {
                info!(%peer, cid = %req.cid, channel_id, "inbound V1 chunk request");
                if let Err(e) = event_tx.try_send(SumNetEvent::ShardRequested {
                    peer_id: peer,
                    request: req,
                    channel_id,
                }) {
                    self.pending_shard_channels.remove(&channel_id);
                    warn!(%e, channel_id, "event channel full — dropping V1 ShardRequested and cleaning up pending channel");
                }
            }
            ShardRequestVersioned::V2(req) => {
                info!(%peer, channel_id, kind = v2_request_kind(&req), "inbound V2 chunk request");
                if let Err(e) = event_tx.try_send(SumNetEvent::ShardRequestedV2 {
                    peer_id: peer,
                    request: req,
                    channel_id,
                }) {
                    self.pending_shard_channels.remove(&channel_id);
                    warn!(%e, channel_id, "event channel full — dropping V2 ShardRequested and cleaning up pending channel");
                }
            }
        }
    }

    fn on_inbound_response(
        &mut self,
        domain: RequestDomain,
        peer: PeerId,
        request_id: request_response::OutboundRequestId,
        response: ShardResponseVersioned,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        // Correlate before anything else looks at the payload. The response is
        // matched against the request record filed under this id **in this
        // domain** — the raw id alone is ambiguous across the two behaviours.
        let key = OutboundKey::new(domain, request_id);
        let origin = match self.outbound.correlate_response(key, peer, &response) {
            Ok(origin) => origin,
            Err(e) => {
                warn!(
                    %peer,
                    %request_id,
                    behaviour = domain.label(),
                    error = %e,
                    "uncorrelated chunk response rejected — dropped before dispatch"
                );
                return;
            }
        };

        match response {
            ShardResponseVersioned::V1(resp) => {
                info!(
                    %peer,
                    cid = %resp.cid,
                    offset = resp.offset,
                    bytes = resp.data.len(),
                    asked = origin.kind().label(),
                    "V1 chunk data received"
                );
                if let Err(e) = event_tx.try_send(SumNetEvent::ShardReceived {
                    peer_id: peer,
                    response: resp,
                    origin,
                }) {
                    warn!(%e, "event channel full — dropping V1 ShardReceived");
                }
            }
            ShardResponseVersioned::V2(resp) => {
                info!(
                    %peer,
                    kind = v2_response_kind(&resp),
                    asked = origin.kind().label(),
                    "V2 response received"
                );
                if let Err(e) = event_tx.try_send(SumNetEvent::ShardReceivedV2 {
                    peer_id: peer,
                    response: resp,
                    origin,
                }) {
                    warn!(%e, "event channel full — dropping V2 ShardReceived");
                }
            }
        }
    }

    fn on_outbound_failure(
        &mut self,
        domain: RequestDomain,
        peer: PeerId,
        request_id: request_response::OutboundRequestId,
        error: &request_response::OutboundFailure,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        // Terminal for this request id: drop the retained record so the pending
        // set stays bounded and the id cannot later be matched.
        let key = OutboundKey::new(domain, request_id);
        let (asked_peer, kind) = match self.outbound.on_failure(key) {
            FailureOutcome::Retained { peer, kind } => (peer, kind),
            FailureOutcome::Unknown => {
                // No record: we cannot say which request this failure is about.
                // Emitting a peer-only failure would invite a consumer to settle
                // every request outstanding to this peer, so it is logged and
                // dropped instead.
                warn!(
                    %peer,
                    %request_id,
                    behaviour = domain.label(),
                    %error,
                    "outbound failure for an untracked request id — not surfaced"
                );
                return;
            }
            FailureOutcome::Poisoned => {
                error!(
                    %peer,
                    %request_id,
                    behaviour = domain.label(),
                    %error,
                    "outbound failure for a poisoned request id — not surfaced"
                );
                return;
            }
        };

        // The failure event carries a peer too. It must agree with the one
        // recorded at send time; a disagreement means the event cannot be
        // attributed and is dropped rather than guessed at.
        if asked_peer != peer {
            warn!(
                %request_id,
                expected = %asked_peer,
                got = %peer,
                %error,
                "outbound failure peer mismatch — not surfaced"
            );
            return;
        }

        warn!(%peer, %error, asked = kind.label(), "chunk request outbound failure");
        // Every field of the event comes from the outbound record.
        let origin = OutboundOrigin::new(asked_peer, kind);
        if let Err(e) = event_tx.try_send(SumNetEvent::ShardRequestFailed {
            peer_id: origin.peer(),
            error: error.to_string(),
            origin,
        }) {
            warn!(%e, "event channel full — dropping ShardRequestFailed");
        }
    }

    // ── Private event dispatcher ──────────────────────────────────────────────

    fn handle_swarm_event(
        &mut self,
        event: SwarmEvent<LocalMeshBehaviourEvent>,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        match event {
            // ── mDNS ──────────────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Mdns(e)) => {
                discovery::handle_mdns_event(
                    e,
                    &mut self.inner.behaviour_mut().gossipsub,
                    &self.policy,
                    event_tx,
                );
            }

            // ── Gossipsub ─────────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Gossipsub(
                gossipsub::Event::Message {
                    propagation_source,
                    message,
                    ..
                },
            )) => {
                let topic = message.topic.to_string();
                let data = message.data;
                info!(
                    from  = %propagation_source,
                    %topic,
                    bytes = data.len(),
                    "gossipsub message received"
                );
                if let Err(e) = event_tx.try_send(SumNetEvent::MessageReceived {
                    from: propagation_source,
                    topic,
                    data,
                }) {
                    warn!(%e, "event channel full — dropping MessageReceived");
                }
            }

            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Gossipsub(e)) => {
                debug!(?e, "gossipsub mesh event");
            }

            // ── Identify ──────────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Identify(
                identify::Event::Received { peer_id, info, .. },
            )) => {
                debug!(%peer_id, "identify received");
                if let Some(l1_addr) =
                    crate::identity::l1_address_from_peer_public_key(&info.public_key)
                    && let Err(e) = event_tx.try_send(SumNetEvent::PeerIdentified {
                        peer_id,
                        l1_address: l1_addr,
                    })
                {
                    warn!(%e, "event channel full — dropping PeerIdentified");
                }

                // Feed the observed-address hint into our external-address
                // candidates. AutoNAT needs this to settle on Public, and
                // DCUtR needs it to advertise a dialable candidate during
                // hole-punch coordination.
                self.inner.add_external_address(info.observed_addr.clone());

                // The peer's advertised listen addresses, minus any the
                // network policy refuses (counted under `identify`). Without
                // an allow-list this is every advertised address, in order.
                // Everything below — relay candidates, Kademlia, the
                // direct-dial shortcut — works from this list only, so an
                // allowed peer cannot slip an alternative address in here.
                let advertised = admitted_listen_addrs(&self.policy, peer_id, &info.listen_addrs);

                // Detect relay-capable peers so we can request a reservation
                // when AutoNAT determines we are Private. The relay hop
                // protocol string contains "relay" — matching works across
                // minor version bumps.
                let supports_relay = info.protocols.iter().any(|p| p.as_ref().contains("relay"));
                if supports_relay {
                    // Identify has confirmed this peer advertises the relay
                    // hop protocol — flip `confirmed = true` so the AutoNAT
                    // handler will consider it for reservations. Merge any
                    // new WAN-dialable addresses with whatever we had from
                    // bootstrap.
                    let entry = self.relay_peers.entry(peer_id).or_default();
                    entry.confirmed = true;
                    let mut added = 0usize;
                    for addr in &advertised {
                        if is_dialable_over_wan(addr) && !entry.addrs.contains(addr) {
                            entry.addrs.push(addr.clone());
                            added += 1;
                        }
                    }
                    debug!(
                        %peer_id,
                        total_addrs = entry.addrs.len(),
                        added,
                        confirmed = entry.confirmed,
                        "identified as relay-capable peer"
                    );
                }

                // Feed identified peer's listen addresses into Kademlia
                // so the DHT routing table populates beyond bootstrap nodes.
                for addr in &advertised {
                    self.inner
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, addr.clone());
                }

                // ── Direct-dial shortcut (Issue #8 candidate D) ────────
                //
                // If we don't already have a direct (non-circuit) connection
                // to this peer, look at the addresses they just advertised
                // for any WAN-dialable, non-circuit candidate and attempt
                // to dial it. This bypasses DCUtR for the asymmetric NAT
                // case (private peer dialing a publicly-reachable peer)
                // by establishing a direct path the moment we learn the
                // public address from Identify.
                //
                // libp2p de-duplicates concurrent dial attempts to the
                // same peer/address pair, and dial errors are non-fatal,
                // so it's safe to call this on every identify event for
                // peers we don't already have a direct connection to.
                if !self.direct_connections.contains_key(&peer_id)
                    && let Some(addr) = pick_direct_dial_candidate(&advertised, peer_id)
                {
                    debug!(%peer_id, %addr, "attempting direct dial (no direct connection yet)");
                    let opts = DialOpts::from(addr);
                    self.policy
                        .label_dial(opts.connection_id(), PolicySource::Autodial);
                    if let Err(e) = self.inner.dial(opts) {
                        debug!(%peer_id, %e, "direct-dial attempt rejected by swarm");
                    }
                }
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Identify(e)) => {
                debug!(?e, "identify event");
            }

            // ── Kademlia DHT ─────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Kademlia(
                kad::Event::RoutingUpdated {
                    peer, addresses, ..
                },
            )) => {
                // Under an allow-list, drop the addresses it refuses (counted
                // under `kad`). Kademlia's table is fed only through filtered
                // paths, so this is a backstop; a peer left with no allowed
                // address is not wired into gossipsub or reported.
                let addrs: Vec<Multiaddr> = addresses
                    .iter()
                    .filter(|a| self.policy.admit(PolicySource::Kad, a, Some(peer)))
                    .cloned()
                    .collect();
                if self.policy.is_restricted() && addrs.is_empty() {
                    debug!(%peer, "Kademlia peer has no address the network policy allows — ignored");
                    return;
                }
                // Wire Kademlia-discovered peers into gossipsub (same as mDNS).
                self.inner
                    .behaviour_mut()
                    .gossipsub
                    .add_explicit_peer(&peer);
                info!(%peer, addr_count = addrs.len(), "Kademlia peer discovered");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerDiscovered {
                    peer_id: peer,
                    addrs,
                }) {
                    warn!(%e, "event channel full — dropping PeerDiscovered (kad)");
                }
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Kademlia(
                kad::Event::OutboundQueryProgressed { result, .. },
            )) => {
                debug!(?result, "Kademlia query progress");
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Kademlia(e)) => {
                debug!(?e, "kademlia event");
            }

            // ── AutoNAT / Relay / DCUtR ──────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Autonat(e)) => {
                nat::handle_autonat_event(
                    e,
                    &self.relay_peers,
                    &mut self.inner,
                    &mut self.nat_status,
                    &mut self.active_relay_reservation,
                    event_tx,
                );
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Relay(e)) => {
                nat::handle_relay_server_event(e);
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::RelayClient(e)) => {
                nat::handle_relay_client_event(e, &mut self.active_relay_reservation, event_tx);
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Dcutr(e)) => {
                nat::handle_dcutr_event(e, event_tx);
            }

            // ── Chunk transfer ────────────────────────────────────────────────
            //
            // Two behaviours, so two sets of arms. Each carries its own
            // `RequestDomain` into the shared handlers below: that domain is
            // both half of the outbound key (raw ids collide across the two
            // behaviours) and the provenance stamped on every inbound response
            // channel.
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXferV1(ev)) => {
                self.on_shard_event(RequestDomain::ShardXferV1, ev, event_tx);
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXferV2(ev)) => {
                self.on_shard_event(RequestDomain::ShardXferV2, ev, event_tx);
            }

            // ── Transport ─────────────────────────────────────────────────────
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(%address, "listening on address");

                // Circuit-relay listen addresses are how NAT'd peers advertise
                // their reachability to the mesh. Register it as an external
                // address so Identify broadcasts it to connected peers — that's
                // the signal other peers use to learn they can dial us via the
                // relay (and that DCUtR can then upgrade that circuit to a
                // direct QUIC connection).
                if address.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                    self.inner.add_external_address(address.clone());
                    debug!(%address, "advertised circuit relay address as external");
                }

                if let Err(e) = event_tx.try_send(SumNetEvent::Listening { addr: address }) {
                    warn!(%e, "event channel full — dropping Listening");
                }
            }

            // A circuit listener can close for three reasons we care about:
            // explicit relay denial, relay-side close (reservation expired /
            // relay shutting down), or local transport failure. In all three
            // cases we need to reset the reservation state machine so the
            // next AutoNAT Private tick is free to retry against another
            // relay candidate. Without this, a one-time denial wedges the
            // node into `Pending(peer)` forever and it never reaches out
            // again.
            SwarmEvent::ListenerClosed {
                addresses, reason, ..
            } => {
                debug!(?addresses, ?reason, "listener closed");
                nat::handle_listener_closed_for_reservation(
                    &addresses,
                    &mut self.active_relay_reservation,
                );
            }

            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                let relayed = endpoint.is_relayed();
                if !relayed {
                    *self.direct_connections.entry(peer_id).or_insert(0) += 1;
                }
                info!(%peer_id, relayed, "connection established");
                debug!(
                    %peer_id,
                    remote = %endpoint.get_remote_address(),
                    outbound = endpoint.is_dialer(),
                    "connection route"
                );
                self.policy.audit(ConnectionAudit::Established {
                    peer_id,
                    remote_addr: endpoint.get_remote_address().clone(),
                    outbound: endpoint.is_dialer(),
                    relayed,
                });
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerConnected { peer_id }) {
                    warn!(%e, "event channel full — dropping PeerConnected");
                }
            }

            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                cause,
                num_established,
                ..
            } => {
                let relayed = endpoint.is_relayed();
                if !relayed
                    && let std::collections::hash_map::Entry::Occupied(mut e) =
                        self.direct_connections.entry(peer_id)
                {
                    let v = e.get_mut();
                    *v = v.saturating_sub(1);
                    if *v == 0 {
                        e.remove();
                    }
                }
                debug!(%peer_id, relayed, ?cause, num_established, "connection closed");
                self.policy.audit(ConnectionAudit::Closed {
                    peer_id,
                    remote_addr: endpoint.get_remote_address().clone(),
                    outbound: endpoint.is_dialer(),
                    relayed,
                });
                if let Some(event) = disconnect_event(peer_id, num_established)
                    && let Err(e) = event_tx.try_send(event)
                {
                    warn!(%e, "event channel full — dropping PeerDisconnected");
                }
            }

            SwarmEvent::IncomingConnectionError { error, .. } => {
                warn!(%error, "incoming connection error");
            }

            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                warn!(?peer_id, %error, "outgoing connection error");
            }

            _ => {}
        }
    }
}

/// The listen addresses for `config`: QUIC always, TCP in WAN mode, on
/// `0.0.0.0` unless `config.listen_ip` says otherwise.
pub(crate) fn listen_addrs(config: &NetConfig) -> Result<Vec<Multiaddr>> {
    let ip = config.listen_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let mut addrs = vec![
        format!("/ip4/{ip}/udp/{}/quic-v1", config.udp_listen_port)
            .parse()
            .context("invalid QUIC listen multiaddr")?,
    ];
    if config.enable_wan {
        addrs.push(
            format!("/ip4/{ip}/tcp/{}", config.tcp_listen_port)
                .parse()
                .context("invalid TCP listen multiaddr")?,
        );
    }
    Ok(addrs)
}

/// The listen addresses a peer advertised through Identify that the network
/// policy admits, in the order advertised. Refused ones are logged and counted
/// under `identify`. Without an allow-list this is `advertised` unchanged.
pub(crate) fn admitted_listen_addrs(
    policy: &NetPolicy,
    peer_id: PeerId,
    advertised: &[Multiaddr],
) -> Vec<Multiaddr> {
    advertised
        .iter()
        .filter(|a| policy.admit(PolicySource::Identify, a, Some(peer_id)))
        .cloned()
        .collect()
}

/// Build the libp2p swarm the way [`SumSwarm`] runs it, without listeners.
///
/// Public so tests can drive the production stack directly. The transport is
/// the stack `SwarmBuilder` produced before SNIP #79 — relay client, then
/// DNS over TCP (Noise, Yamux) and QUIC, behind a 10 s connection timeout —
/// composed by hand so that [`PolicyTransport`] can sit outside all of it.
/// Without an allow-list the gate passes every call straight through.
pub fn build_swarm(
    config: &NetConfig,
    keypair: Keypair,
    policy: Arc<NetPolicy>,
) -> Result<libp2p::Swarm<LocalMeshBehaviour>> {
    let local_peer_id = keypair.public().to_peer_id();

    let gossip_cfg = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(gossipsub::ValidationMode::Strict)
        .history_length(10)
        .history_gossip(3)
        .build()
        .map_err(|msg| anyhow::anyhow!("gossipsub config error: {msg}"))?;

    // Unified transport chain: TCP/Noise/Yamux + QUIC + DNS + relay-client.
    // Relay circuits (v2) run over TCP, so TCP is mandatory whenever
    // relays are used. In LAN-only mode we still build all three
    // transports but bind only the QUIC listener and never bootstrap
    // the DHT, so the WAN transports stay idle.
    //
    // DNS wraps TCP and QUIC so `/dns4/host/...` multiaddrs (used in our
    // bootstrap peers) actually resolve. Without it, dials against any DNS
    // multiaddr fail at the swarm with "Multiaddr is not supported" and the
    // node can never reach the relay on a fresh startup.
    let (transport, relay_client) = build_transport(&keypair, policy.clone())?;

    // Disabled mDNS is not constructed at all: no socket, no announcement.
    let mdns = if policy.mdns_enabled() {
        Some(mdns::tokio::Behaviour::new(
            mdns::Config::default(),
            local_peer_id,
        )?)
    } else {
        None
    };

    let gossipsub_behaviour = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(keypair.clone()),
        gossip_cfg,
    )
    .map_err(|msg| anyhow::anyhow!("gossipsub behaviour error: {msg}"))?;

    let identify = identify::Behaviour::new(identify::Config::new(
        "/sum-node/0.1.0".into(),
        keypair.public(),
    ));

    // Two behaviours, one protocol each. `send_request` attaches every
    // protocol its behaviour was registered with to every request, so a
    // single behaviour carrying both protocols cannot be asked to speak one
    // of them — the version has to be a choice of behaviour. See
    // `crate::behaviour` for the full argument.
    let shard_xfer_v1 = build_shard_xfer_v1();
    let shard_xfer_v2 = build_shard_xfer_v2();

    let kademlia = discovery::build_kademlia(local_peer_id);
    let autonat = nat::build_autonat(local_peer_id);
    // Drives whether the relay server accepts reservations (opt-in, only on
    // publicly-reachable hosts).
    let relay = nat::build_relay_server(local_peer_id, config.relay_server);
    let dcutr = dcutr::Behaviour::new(local_peer_id);

    let behaviour = LocalMeshBehaviour {
        net_policy: PolicyBehaviour::new(policy.clone()),
        mdns: Gated::new(Toggle::from(mdns), PolicySource::Mdns, policy.clone()),
        gossipsub: gossipsub_behaviour,
        identify: Gated::new(identify, PolicySource::Identify, policy.clone()),
        shard_xfer_v1,
        shard_xfer_v2,
        kademlia: Gated::new(kademlia, PolicySource::Kad, policy.clone()),
        autonat: Gated::new(autonat, PolicySource::Autodial, policy.clone()),
        relay,
        relay_client: Gated::new(relay_client, PolicySource::Autodial, policy.clone()),
        dcutr: Gated::new(dcutr, PolicySource::Autodial, policy),
    };

    Ok(libp2p::Swarm::new(
        transport,
        behaviour,
        local_peer_id,
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT),
    ))
}

/// The transport stack, with the policy gate outermost, plus the relay-client
/// behaviour that pairs with the relay transport.
fn build_transport(
    keypair: &Keypair,
    policy: Arc<NetPolicy>,
) -> Result<(Boxed<(PeerId, StreamMuxerBox)>, relay::client::Behaviour)> {
    let tcp = libp2p::tcp::tokio::Transport::new(libp2p::tcp::Config::default())
        .upgrade(Version::V1Lazy)
        .authenticate(libp2p::noise::Config::new(keypair)?)
        .multiplex(libp2p::yamux::Config::default())
        .map(|(p, c), _| (p, StreamMuxerBox::new(c)));
    let quic = libp2p::quic::tokio::Transport::new(libp2p::quic::Config::new(keypair))
        .map(|(p, m), _| (p, StreamMuxerBox::new(m)));
    let tcp_quic = tcp.or_transport(quic).map(|either, _| either.into_inner());
    let dns = libp2p::dns::tokio::Transport::system(tcp_quic)?;

    let (relay_transport, relay_client) = relay::client::new(keypair.public().to_peer_id());
    let relay_transport = relay_transport
        .upgrade(Version::V1Lazy)
        .authenticate(libp2p::noise::Config::new(keypair)?)
        .multiplex(libp2p::yamux::Config::default())
        .map(|(p, c), _| (p, StreamMuxerBox::new(c)));

    let stack = relay_transport
        .or_transport(dns)
        .map(|either, _| either.into_inner());
    let stack = TransportTimeout::new(stack, CONNECTION_TIMEOUT).boxed();

    Ok((PolicyTransport::new(stack, policy).boxed(), relay_client))
}

/// Return `true` if `addr` is a WAN-dialable multiaddr.
///
/// Filters out loopback, RFC1918 private ranges, link-local, and the
/// CGNAT `100.64.0.0/10` block for IPv4; loopback, link-local, and ULA
/// (`fc00::/7`) for IPv6. DNS-based addresses (`/dns/`, `/dns4/`, `/dns6/`,
/// `/dnsaddr/`) are accepted as-is — we trust operators to advertise names
/// that resolve to real WAN addresses, and if resolution yields a private
/// address the subsequent dial will simply fail.
///
/// Addresses with no host component (e.g. bare `/p2p/<peer>/p2p-circuit`)
/// return `false` — the helper only endorses addresses that carry enough
/// information to actually dial.
pub(crate) fn is_dialable_over_wan(addr: &Multiaddr) -> bool {
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(ip) => {
                // Loopback 127.0.0.0/8
                if ip.is_loopback() {
                    return false;
                }
                // Link-local 169.254.0.0/16
                if ip.is_link_local() {
                    return false;
                }
                // Unspecified 0.0.0.0
                if ip.is_unspecified() {
                    return false;
                }
                // RFC1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                let octets = ip.octets();
                if octets[0] == 10 {
                    return false;
                }
                if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                    return false;
                }
                if octets[0] == 192 && octets[1] == 168 {
                    return false;
                }
                // CGNAT 100.64.0.0/10
                if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                    return false;
                }
                return true;
            }
            Protocol::Ip6(ip) => {
                if ip.is_loopback() {
                    return false;
                }
                if ip.is_unspecified() {
                    return false;
                }
                // Link-local fe80::/10
                let segs = ip.segments();
                if segs[0] & 0xffc0 == 0xfe80 {
                    return false;
                }
                // ULA fc00::/7
                if segs[0] & 0xfe00 == 0xfc00 {
                    return false;
                }
                return true;
            }
            // DNS-backed host components. We can't cheaply validate where
            // they resolve; treating them as dialable lets operators point
            // peers at stable relay hostnames (the common production shape).
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Dnsaddr(_) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Pick the first WAN-dialable, non-circuit address from a peer's
/// advertised `listen_addrs` and return it suffixed with `/p2p/<peer_id>`.
///
/// Used by the identify handler to bypass DCUtR when we already know a
/// public address for the remote peer — e.g. asymmetric NAT, where one
/// peer is publicly reachable and the other only needs to dial outbound.
///
/// Returns `None` if no usable candidate is found. Filters out:
/// - LAN / loopback / RFC1918 / link-local / CGNAT / ULA (via [`is_dialable_over_wan`])
/// - Addresses already containing a `/p2p-circuit` component (we don't
///   want to re-establish a circuit; if the peer is *only* reachable via
///   relay, we already have that path).
pub(crate) fn pick_direct_dial_candidate(
    listen_addrs: &[Multiaddr],
    peer_id: PeerId,
) -> Option<Multiaddr> {
    for addr in listen_addrs {
        if !is_dialable_over_wan(addr) {
            continue;
        }
        if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
            continue;
        }
        // Build the dialable form: ensure `/p2p/<peer>` is present so
        // libp2p can verify the remote peer id during connection setup.
        let already_has_p2p = addr.iter().any(|p| matches!(p, Protocol::P2p(_)));
        let full_addr = if already_has_p2p {
            addr.clone()
        } else {
            addr.clone().with(Protocol::P2p(peer_id))
        };
        return Some(full_addr);
    }
    None
}

/// Reap entries from a pending-channel map whose insertion time exceeds `timeout`.
/// Returns the number of entries removed. Extracted for testability.
pub(crate) fn reap_stale_entries<V>(
    map: &mut HashMap<u64, (V, Instant)>,
    timeout: Duration,
) -> usize {
    let now = Instant::now();
    let before = map.len();
    map.retain(|_id, (_v, inserted)| now.duration_since(*inserted) <= timeout);
    before - map.len()
}

/// Compact label for a V2 request, used in tracing only.
fn v2_request_kind(req: &ShardRequestV2) -> &'static str {
    match req {
        ShardRequestV2::Pull { .. } => "Pull",
        ShardRequestV2::Push { .. } => "Push",
        ShardRequestV2::ManifestPush { .. } => "ManifestPush",
        ShardRequestV2::ManifestPull { .. } => "ManifestPull",
    }
}

/// Compact label for a V2 response, used in tracing only.
fn v2_response_kind(resp: &ShardResponseV2) -> &'static str {
    match resp {
        ShardResponseV2::Data { .. } => "Data",
        ShardResponseV2::PushAck { .. } => "PushAck",
        ShardResponseV2::ManifestPushAck { .. } => "ManifestPushAck",
        ShardResponseV2::ManifestData { .. } => "ManifestData",
    }
}

/// The event a closed connection produces. `remaining` is the number of
/// other connections to the same peer that are still established; only the
/// last one closing is a disconnect (see [`SumNetEvent::PeerDisconnected`]).
fn disconnect_event(peer_id: PeerId, remaining: u32) -> Option<SumNetEvent> {
    (remaining == 0).then_some(SumNetEvent::PeerDisconnected { peer_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // A trivial stand-in for ResponseChannel (which we can't construct).
    // reap_stale_entries is generic over V, so any type works.

    // ── Response-channel provenance ──────────────────────────────────────
    //
    // `ResponseChannel` cannot be constructed outside libp2p, and it does not
    // need to be: the decision under test is "does the domain filed with this
    // channel match the domain asking for it", which is independent of what
    // the channel is. `take_channel` is generic for exactly that reason, so
    // these use `String` and exercise the real function production calls.

    fn filed(
        domain: RequestDomain,
        channel: &str,
    ) -> HashMap<u64, (PendingChannel<String>, Instant)> {
        HashMap::from([(
            7,
            (
                PendingChannel {
                    domain,
                    channel: channel.to_string(),
                },
                Instant::now(),
            ),
        )])
    }

    #[test]
    fn a_channel_is_taken_by_the_domain_it_arrived_on() {
        let mut m = filed(RequestDomain::ShardXferV1, "v1-channel");
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV1),
            ChannelTake::Taken("v1-channel".to_string())
        );
        assert!(m.is_empty(), "a taken channel is spent");
    }

    /// A V2 responder must not be handed a channel that arrived on the V1
    /// behaviour. Both are `ResponseChannel<ShardResponseVersioned>`, so
    /// nothing but this check stands between the two.
    #[test]
    fn a_v1_channel_is_refused_to_the_v2_domain() {
        let mut m = filed(RequestDomain::ShardXferV1, "v1-channel");
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV2),
            ChannelTake::WrongDomain {
                have: RequestDomain::ShardXferV1
            }
        );
        assert_eq!(m.len(), 1, "a refused channel is left for its own domain");

        // And the domain it belongs to can still answer on it.
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV1),
            ChannelTake::Taken("v1-channel".to_string())
        );
    }

    /// The mirror image, because a check that only runs one way is half a
    /// check.
    #[test]
    fn a_v2_channel_is_refused_to_the_v1_domain() {
        let mut m = filed(RequestDomain::ShardXferV2, "v2-channel");
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV1),
            ChannelTake::WrongDomain {
                have: RequestDomain::ShardXferV2
            }
        );
        assert_eq!(m.len(), 1);
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV2),
            ChannelTake::Taken("v2-channel".to_string())
        );
    }

    #[test]
    fn an_unknown_channel_id_is_missing_not_mismatched() {
        let mut m = filed(RequestDomain::ShardXferV1, "v1-channel");
        assert_eq!(
            take_channel(&mut m, 999, RequestDomain::ShardXferV1),
            ChannelTake::Missing
        );
        assert_eq!(
            take_channel(&mut m, 999, RequestDomain::ShardXferV2),
            ChannelTake::Missing
        );
    }

    /// A channel is spent once. A second responder — of either domain — gets
    /// `Missing`, never the channel again.
    #[test]
    fn a_channel_cannot_be_taken_twice() {
        let mut m = filed(RequestDomain::ShardXferV2, "v2-channel");
        assert!(matches!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV2),
            ChannelTake::Taken(_)
        ));
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV2),
            ChannelTake::Missing
        );
        assert_eq!(
            take_channel(&mut m, 7, RequestDomain::ShardXferV1),
            ChannelTake::Missing
        );
    }

    /// The counters are fixed struct fields. Nothing a peer supplies can
    /// become a key, because there are no keys.
    #[test]
    fn refusal_counters_start_at_zero_and_are_plain_fields() {
        let r = SwarmRefusals::default();
        assert_eq!(r.v1_response_on_v2_channel, 0);
        assert_eq!(r.v2_response_on_v1_channel, 0);
        assert_eq!(r.response_without_channel, 0);
        assert_eq!(r.outbound_domain_protocol_mismatch, 0);
        assert_eq!(r.outbound_id_collision, 0);
        assert_eq!(r.inbound_version_domain_mismatch, 0);
    }

    #[test]
    fn reap_stale_entries_removes_expired() {
        let mut map: HashMap<u64, (String, Instant)> = HashMap::new();

        // Insert one "old" entry (expired) and one "fresh" entry.
        let old_time = Instant::now() - Duration::from_secs(200);
        let fresh_time = Instant::now();
        map.insert(1, ("old".into(), old_time));
        map.insert(2, ("fresh".into(), fresh_time));

        let reaped = reap_stale_entries(&mut map, PENDING_CHANNEL_TIMEOUT);
        assert_eq!(reaped, 1);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&2));
        assert!(!map.contains_key(&1));
    }

    #[test]
    fn reap_stale_entries_nothing_to_reap() {
        let mut map: HashMap<u64, (String, Instant)> = HashMap::new();
        map.insert(1, ("a".into(), Instant::now()));
        map.insert(2, ("b".into(), Instant::now()));

        let reaped = reap_stale_entries(&mut map, PENDING_CHANNEL_TIMEOUT);
        assert_eq!(reaped, 0);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn reap_stale_entries_all_expired() {
        let mut map: HashMap<u64, (String, Instant)> = HashMap::new();
        let old = Instant::now() - Duration::from_secs(300);
        for i in 0..5 {
            map.insert(i, (format!("ch-{i}"), old));
        }

        let reaped = reap_stale_entries(&mut map, PENDING_CHANNEL_TIMEOUT);
        assert_eq!(reaped, 5);
        assert!(map.is_empty());
    }

    #[test]
    fn reap_stale_entries_empty_map() {
        let mut map: HashMap<u64, (String, Instant)> = HashMap::new();
        let reaped = reap_stale_entries(&mut map, PENDING_CHANNEL_TIMEOUT);
        assert_eq!(reaped, 0);
    }

    #[test]
    fn reap_stale_entries_recent_not_expired() {
        let mut map: HashMap<u64, (String, Instant)> = HashMap::new();
        // Well within the timeout — should NOT be reaped.
        let recent = Instant::now() - Duration::from_secs(60);
        map.insert(1, ("recent".into(), recent));

        let reaped = reap_stale_entries(&mut map, PENDING_CHANNEL_TIMEOUT);
        assert_eq!(reaped, 0);
        assert_eq!(map.len(), 1);
    }

    // ── is_dialable_over_wan ──────────────────────────────────────────

    #[test]
    fn is_dialable_over_wan_matrix() {
        fn ma(s: &str) -> Multiaddr {
            s.parse().expect("test multiaddr must parse")
        }

        // Public IPv4 → dialable.
        assert!(is_dialable_over_wan(&ma("/ip4/8.8.8.8/tcp/4001")));
        assert!(is_dialable_over_wan(&ma("/ip4/164.92.93.224/tcp/4001")));
        assert!(is_dialable_over_wan(&ma("/ip4/1.2.3.4/udp/4001/quic-v1")));

        // Loopback → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip4/127.0.0.1/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma(
            "/ip4/127.1.2.3/udp/4001/quic-v1"
        )));

        // RFC1918 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip4/10.0.0.1/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma("/ip4/172.16.0.1/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma("/ip4/172.31.255.254/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma("/ip4/192.168.1.1/tcp/4001")));

        // 172.32.0.1 is NOT RFC1918 (only 172.16–172.31 is private).
        assert!(is_dialable_over_wan(&ma("/ip4/172.32.0.1/tcp/4001")));

        // CGNAT 100.64.0.0/10 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip4/100.64.0.1/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma("/ip4/100.127.255.254/tcp/4001")));

        // 100.63.x.x and 100.128.x.x are NOT CGNAT — they're public.
        assert!(is_dialable_over_wan(&ma("/ip4/100.63.0.1/tcp/4001")));
        assert!(is_dialable_over_wan(&ma("/ip4/100.128.0.1/tcp/4001")));

        // Link-local 169.254.0.0/16 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip4/169.254.1.1/tcp/4001")));

        // Unspecified 0.0.0.0 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip4/0.0.0.0/tcp/4001")));

        // IPv6 loopback → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip6/::1/tcp/4001")));

        // IPv6 link-local fe80::/10 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip6/fe80::1/tcp/4001")));

        // IPv6 ULA fc00::/7 → not dialable.
        assert!(!is_dialable_over_wan(&ma("/ip6/fc00::1/tcp/4001")));
        assert!(!is_dialable_over_wan(&ma("/ip6/fd00::1/tcp/4001")));

        // Public IPv6 → dialable.
        assert!(is_dialable_over_wan(&ma("/ip6/2001:db8::1/tcp/4001")));

        // Bare /p2p/... with no IP → not dialable.
        let peer_id_str = "12D3KooWDbWRosFwyo6oPW2vw57y3dc8zLqixyPpxSUKQ5qUeiSc";
        assert!(!is_dialable_over_wan(&ma(&format!("/p2p/{peer_id_str}"))));

        // ── DNS-backed addresses MUST be dialable (Codex fix) ──
        //
        // Operators commonly advertise stable relay hostnames like
        // `/dns4/relay.example.com/tcp/4001`. Filtering these out at the
        // WAN helper drops them before the reservation logic ever sees
        // them — which is the entire defect we're guarding against here.
        assert!(is_dialable_over_wan(&ma(
            "/dns4/relay.example.com/tcp/4001"
        )));
        assert!(is_dialable_over_wan(&ma(
            "/dns4/relay.example.com/udp/4001/quic-v1"
        )));
        assert!(is_dialable_over_wan(&ma(
            "/dns6/relay.example.com/tcp/4001"
        )));
        assert!(is_dialable_over_wan(&ma("/dns/relay.example.com/tcp/4001")));
        assert!(is_dialable_over_wan(&ma("/dnsaddr/relay.example.com")));

        // DNS with /p2p/<peer> appended — still dialable (common bootstrap form).
        let relay_peer = "12D3KooWDbWRosFwyo6oPW2vw57y3dc8zLqixyPpxSUKQ5qUeiSc";
        assert!(is_dialable_over_wan(&ma(&format!(
            "/dns4/bootstrap.example.com/tcp/4001/p2p/{relay_peer}"
        ))));
    }

    // ── pick_direct_dial_candidate ───────────────────────────────────────

    fn rand_peer() -> PeerId {
        // Generate a unique peer id from a fresh keypair. Using libp2p's
        // ed25519 because that's what the rest of the crate uses.
        let kp = libp2p::identity::Keypair::generate_ed25519();
        kp.public().to_peer_id()
    }

    fn ma(s: &str) -> Multiaddr {
        s.parse().expect("test multiaddr must parse")
    }

    #[test]
    fn pick_direct_dial_returns_first_wan_addr_with_p2p_appended() {
        let peer = rand_peer();
        let addrs = vec![
            ma("/ip4/127.0.0.1/tcp/4001"),       // loopback — skip
            ma("/ip4/192.168.1.5/tcp/4001"),     // RFC1918 — skip
            ma("/ip4/8.8.8.8/tcp/4001"),         // public — pick this
            ma("/ip4/1.2.3.4/udp/4001/quic-v1"), // also public — would also work
        ];
        let picked = pick_direct_dial_candidate(&addrs, peer).expect("should pick a candidate");
        // First WAN-dialable address wins.
        assert_eq!(
            picked.to_string(),
            format!("/ip4/8.8.8.8/tcp/4001/p2p/{peer}")
        );
    }

    #[test]
    fn pick_direct_dial_skips_circuit_addresses() {
        let peer = rand_peer();
        let relay = rand_peer();
        let addrs = vec![
            // Circuit address — must NOT be picked.
            ma(&format!(
                "/ip4/164.92.93.224/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{peer}"
            )),
            // Direct WAN address — should be the choice.
            ma("/ip4/172.91.65.115/tcp/4001"),
        ];
        let picked =
            pick_direct_dial_candidate(&addrs, peer).expect("should fall through to direct");
        assert_eq!(
            picked.to_string(),
            format!("/ip4/172.91.65.115/tcp/4001/p2p/{peer}")
        );
    }

    #[test]
    fn pick_direct_dial_returns_none_when_only_lan_or_circuit() {
        let peer = rand_peer();
        let relay = rand_peer();
        let addrs = vec![
            ma("/ip4/192.168.1.5/tcp/4001"),
            ma("/ip4/10.0.0.166/tcp/4001"),
            ma(&format!(
                "/ip4/164.92.93.224/tcp/4001/p2p/{relay}/p2p-circuit"
            )),
        ];
        assert!(pick_direct_dial_candidate(&addrs, peer).is_none());
    }

    #[test]
    fn pick_direct_dial_preserves_existing_p2p_segment() {
        // If the address already carries `/p2p/<peer>`, we MUST NOT append
        // a duplicate.
        let peer = rand_peer();
        let addrs = vec![ma(&format!("/ip4/8.8.8.8/tcp/4001/p2p/{peer}"))];
        let picked = pick_direct_dial_candidate(&addrs, peer).unwrap();

        // Exactly one P2p component.
        let p2p_count = picked
            .iter()
            .filter(|p| matches!(p, Protocol::P2p(_)))
            .count();
        assert_eq!(p2p_count, 1);
        assert_eq!(
            picked.to_string(),
            format!("/ip4/8.8.8.8/tcp/4001/p2p/{peer}")
        );
    }

    #[test]
    fn pick_direct_dial_accepts_dns_addresses() {
        let peer = rand_peer();
        let addrs = vec![ma("/dns4/relay.example.com/tcp/4001")];
        let picked = pick_direct_dial_candidate(&addrs, peer).unwrap();
        assert_eq!(
            picked.to_string(),
            format!("/dns4/relay.example.com/tcp/4001/p2p/{peer}")
        );
    }

    #[test]
    fn pick_direct_dial_empty_input_returns_none() {
        let peer = rand_peer();
        assert!(pick_direct_dial_candidate(&[], peer).is_none());
    }

    #[test]
    fn pick_direct_dial_quic_address_is_picked() {
        let peer = rand_peer();
        let addrs = vec![ma("/ip4/8.8.8.8/udp/4001/quic-v1")];
        let picked = pick_direct_dial_candidate(&addrs, peer).unwrap();
        assert_eq!(
            picked.to_string(),
            format!("/ip4/8.8.8.8/udp/4001/quic-v1/p2p/{peer}")
        );
    }

    // ── Network policy (SNIP #79) ────────────────────────────────────────

    fn restricted(entries: &[&str]) -> NetPolicy {
        NetPolicy::new(true, crate::policy::AddrPolicy::parse(entries).unwrap())
    }

    /// Listeners are what they were before #79: QUIC on 0.0.0.0 always, TCP
    /// only in WAN mode. The policy fields do not touch them.
    #[test]
    fn listen_addrs_are_unchanged_by_default_and_by_the_policy() {
        let mut cfg = NetConfig::default();
        assert_eq!(
            listen_addrs(&cfg).unwrap(),
            vec![ma("/ip4/0.0.0.0/udp/0/quic-v1")]
        );
        cfg.enable_wan = true;
        cfg.udp_listen_port = 4001;
        cfg.tcp_listen_port = 4002;
        assert_eq!(
            listen_addrs(&cfg).unwrap(),
            vec![
                ma("/ip4/0.0.0.0/udp/4001/quic-v1"),
                ma("/ip4/0.0.0.0/tcp/4002")
            ]
        );
        cfg.mdns = false;
        cfg.outbound_allow = vec!["/ip4/127.0.0.1/tcp/9".into()];
        assert_eq!(
            listen_addrs(&cfg).unwrap(),
            vec![
                ma("/ip4/0.0.0.0/udp/4001/quic-v1"),
                ma("/ip4/0.0.0.0/tcp/4002")
            ],
            "the policy restricts dials, not listeners"
        );
        cfg.listen_ip = Some(Ipv4Addr::LOCALHOST);
        assert_eq!(
            listen_addrs(&cfg).unwrap(),
            vec![
                ma("/ip4/127.0.0.1/udp/4001/quic-v1"),
                ma("/ip4/127.0.0.1/tcp/4002")
            ]
        );
    }

    #[test]
    fn identify_addresses_pass_unchanged_without_a_policy() {
        let peer = rand_peer();
        let advertised = vec![
            ma("/ip4/203.0.113.10/tcp/4001"),
            ma("/ip4/127.0.0.1/udp/4001/quic-v1"),
        ];
        let open = NetPolicy::new(true, None);
        assert_eq!(admitted_listen_addrs(&open, peer, &advertised), advertised);
        assert_eq!(open.refusals().total(), 0);
    }

    /// An allowed peer advertising an alternative public address through
    /// Identify: the alternative is refused, so the direct-dial shortcut
    /// (autodial) never sees it, and an allowed address is still picked.
    #[test]
    fn identify_alternatives_are_refused_before_the_direct_dial_shortcut() {
        let peer = rand_peer();
        let policy = restricted(&["/ip4/203.0.113.10/tcp/4001"]);
        let advertised = vec![
            ma("/ip4/198.51.100.7/tcp/4001"),
            ma("/ip4/198.51.100.7/udp/4001/quic-v1"),
            ma("/ip4/203.0.113.10/tcp/4001"),
        ];
        let admitted = admitted_listen_addrs(&policy, peer, &advertised);
        assert_eq!(admitted, vec![ma("/ip4/203.0.113.10/tcp/4001")]);
        assert_eq!(
            pick_direct_dial_candidate(&admitted, peer).unwrap(),
            ma(&format!("/ip4/203.0.113.10/tcp/4001/p2p/{peer}"))
        );
        assert_eq!(policy.refusals().identify, 2);

        // Only alternatives advertised: nothing to dial.
        let none = admitted_listen_addrs(&policy, peer, &advertised[..2]);
        assert!(pick_direct_dial_candidate(&none, peer).is_none());
        assert_eq!(policy.refusals().identify, 4);
    }

    /// Closing one of several connections to a peer leaves it connected (and
    /// identified); only the last close is a disconnect.
    #[test]
    fn only_the_last_connection_close_is_a_disconnect() {
        let peer = rand_peer();
        assert!(disconnect_event(peer, 1).is_none());
        assert!(disconnect_event(peer, 3).is_none());
        match disconnect_event(peer, 0) {
            Some(SumNetEvent::PeerDisconnected { peer_id }) => assert_eq!(peer_id, peer),
            other => panic!("expected PeerDisconnected, got {other:?}"),
        }
    }
}
