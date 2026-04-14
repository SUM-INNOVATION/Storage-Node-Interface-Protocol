use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    dcutr, gossipsub, identify, kad, mdns,
    identity::Keypair,
    request_response::{self, ProtocolSupport, ResponseChannel},
    swarm::SwarmEvent,
    Multiaddr, PeerId, SwarmBuilder,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use sum_types::config::NetConfig;

use crate::{
    behaviour::{LocalMeshBehaviour, LocalMeshBehaviourEvent},
    codec::{ShardRequest, ShardResponse, SHARD_XFER_PROTOCOL},
    discovery,
    events::SumNetEvent,
    gossip::GossipManager,
    nat,
};

/// How long a pending response channel is kept before it is considered orphaned.
/// Matches the request-response timeout (120s) so the requester has already
/// given up by the time we reap.
const PENDING_CHANNEL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the reaper runs to clean up orphaned pending channels.
const REAPER_INTERVAL: Duration = Duration::from_secs(30);

// ── SwarmCommand ──────────────────────────────────────────────────────────────

/// Commands sent from the [`crate::SumNet`] handle into the running swarm loop.
#[derive(Debug)]
pub enum SwarmCommand {
    /// Publish bytes to a named Gossipsub topic.
    Publish { topic: String, data: Vec<u8> },

    /// Send a chunk request to a remote peer.
    RequestShard { peer_id: PeerId, request: ShardRequest },

    /// Push a chunk to a remote peer using a shared, ref-counted buffer.
    ///
    /// Multiple replicas can share the same `Arc<[u8]>` payload — the
    /// command channel only buffers cheap pointer clones, not full copies
    /// of the chunk. The `Vec<u8>` materialization happens once, in the
    /// command handler, immediately before libp2p serializes the request.
    PushShard {
        peer_id: PeerId,
        cid: String,
        data: Arc<[u8]>,
    },

    /// Send a chunk response on a stored response channel.
    SendShardResponse { channel_id: u64, response: ShardResponse },

    /// Exit the event loop cleanly.
    Shutdown,
}

// ── SumSwarm ─────────────────────────────────────────────────────────────────

/// Owns the [`libp2p::Swarm`] and the [`GossipManager`].
/// Constructed by [`SumSwarm::build`] and consumed by [`SumSwarm::run`].
pub struct SumSwarm {
    inner:  libp2p::Swarm<LocalMeshBehaviour>,
    gossip: GossipManager,

    /// Stores response channels received from inbound chunk requests,
    /// together with the insertion time so orphaned entries can be reaped.
    pending_shard_channels: HashMap<u64, (ResponseChannel<ShardResponse>, Instant)>,

    /// Monotonic counter for channel IDs.
    next_channel_id: u64,

    /// Peers that advertise the Circuit Relay v2 hop protocol, discovered
    /// via Identify. These are the candidates we request reservations from
    /// when AutoNAT determines we are Private.
    relay_peers: Vec<PeerId>,

    /// Current NAT status as determined by AutoNAT.
    nat_status: nat::NatStatus,

    /// The relay peer we currently hold a reservation with, if any.
    /// Prevents reservation spam on flapping NAT status.
    active_relay_reservation: Option<PeerId>,
}

impl SumSwarm {
    /// Construct and configure the Swarm with an externally-provided keypair.
    ///
    /// The keypair should be derived from the user's SUM Chain L1 wallet seed
    /// via [`crate::identity::keypair_from_seed`].
    ///
    /// Transport:  QUIC always; TCP+Noise+Yamux added when `enable_wan` is true.
    /// Behaviour:  mDNS + Gossipsub + Identify + chunk transfer + Kademlia DHT.
    /// Listener:   `0.0.0.0:<listen_port>` (QUIC) + `0.0.0.0:<tcp_listen_port>` (TCP, if WAN).
    pub fn build(config: &NetConfig, keypair: Keypair) -> Result<Self> {
        let gossip_cfg = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(10))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .history_length(10)
            .history_gossip(3)
            .build()
            .map_err(|msg| anyhow::anyhow!("gossipsub config error: {msg}"))?;

        // Captured by the behaviour closure; drives whether the relay server
        // accepts reservations (opt-in, only on publicly-reachable hosts).
        let relay_server_enabled = config.relay_server;

        // Unified transport chain: TCP/Noise/Yamux + QUIC + relay-client.
        // Relay circuits (v2) run over TCP, so TCP is mandatory whenever
        // relays are used. In LAN-only mode we still build all three
        // transports but bind only the QUIC listener and never bootstrap
        // the DHT, so the WAN transports stay idle.
        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_behaviour(|key, relay_client| -> std::result::Result<LocalMeshBehaviour, Box<dyn std::error::Error + Send + Sync>> {
                let local_peer_id = key.public().to_peer_id();

                let mdns = mdns::tokio::Behaviour::new(
                    mdns::Config::default(),
                    local_peer_id,
                )?;

                let gossipsub_behaviour = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg.clone(),
                )
                .map_err(|msg| -> Box<dyn std::error::Error + Send + Sync> {
                    msg.into()
                })?;

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/sum-node/0.1.0".into(),
                    key.public(),
                ));

                let shard_xfer = request_response::Behaviour::new(
                    [(SHARD_XFER_PROTOCOL.to_string(), ProtocolSupport::Full)],
                    request_response::Config::default()
                        .with_request_timeout(Duration::from_secs(120)),
                );

                let kademlia = discovery::build_kademlia(local_peer_id);
                let autonat  = nat::build_autonat(local_peer_id);
                let relay    = nat::build_relay_server(local_peer_id, relay_server_enabled);
                let dcutr    = dcutr::Behaviour::new(local_peer_id);

                Ok(LocalMeshBehaviour {
                    mdns,
                    gossipsub: gossipsub_behaviour,
                    identify,
                    shard_xfer,
                    kademlia,
                    autonat,
                    relay,
                    relay_client,
                    dcutr,
                })
            })?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        // QUIC listener (always)
        let quic_addr: Multiaddr =
            format!("/ip4/0.0.0.0/udp/{}/quic-v1", config.listen_port)
                .parse()
                .context("invalid QUIC listen multiaddr")?;
        swarm
            .listen_on(quic_addr)
            .context("failed to bind QUIC listener")?;

        // TCP listener (WAN mode only)
        if config.enable_wan {
            let tcp_addr: Multiaddr =
                format!("/ip4/0.0.0.0/tcp/{}", config.tcp_listen_port)
                    .parse()
                    .context("invalid TCP listen multiaddr")?;
            swarm
                .listen_on(tcp_addr)
                .context("failed to bind TCP listener")?;
        }

        Ok(Self {
            inner:  swarm,
            gossip: GossipManager::new(),
            pending_shard_channels: HashMap::new(),
            next_channel_id: 0,
            relay_peers: Vec::new(),
            nat_status: nat::NatStatus::Unknown,
            active_relay_reservation: None,
        })
    }

    /// Bootstrap Kademlia DHT with the provided peer multiaddrs.
    ///
    /// Each address must end with `/p2p/<peer_id>`. The node dials the
    /// bootstrap peers and initiates a Kademlia bootstrap query.
    pub fn bootstrap_kademlia(&mut self, bootstrap_peers: &[String]) -> Result<()> {
        for addr_str in bootstrap_peers {
            let addr: Multiaddr = addr_str.parse()
                .context(format!("invalid bootstrap multiaddr: {addr_str}"))?;

            // Extract PeerId from the last /p2p/<peer_id> component.
            let peer_id = addr.iter()
                .find_map(|proto| {
                    if let libp2p::multiaddr::Protocol::P2p(pid) = proto {
                        Some(pid)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("bootstrap addr missing /p2p/ component: {addr_str}"))?;

            self.inner.behaviour_mut().kademlia
                .add_address(&peer_id, addr.clone());

            info!(%peer_id, %addr, "added Kademlia bootstrap peer");

            if let Err(e) = self.inner.dial(addr) {
                warn!(%peer_id, %e, "failed to dial bootstrap peer");
            }
        }

        if !bootstrap_peers.is_empty() {
            self.inner.behaviour_mut().kademlia.bootstrap()
                .map_err(|e| anyhow::anyhow!("Kademlia bootstrap failed: {e}"))?;
            info!(peers = bootstrap_peers.len(), "Kademlia bootstrap initiated");
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
        event_tx:   mpsc::Sender<SumNetEvent>,
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
                            self.inner.behaviour_mut().shard_xfer
                                .send_request(&peer_id, request);
                        }
                        Some(SwarmCommand::PushShard { peer_id, cid, data }) => {
                            // Materialize the Vec<u8> exactly once, here at
                            // the libp2p hand-off. The Arc<[u8]> may have been
                            // cloned R times upstream, but those clones share
                            // a single backing buffer until this point.
                            let request = ShardRequest {
                                cid,
                                offset: None,
                                max_bytes: None,
                                push_data: Some(data.to_vec()),
                            };
                            self.inner.behaviour_mut().shard_xfer
                                .send_request(&peer_id, request);
                        }
                        Some(SwarmCommand::SendShardResponse { channel_id, response }) => {
                            if let Some((channel, _inserted)) = self.pending_shard_channels.remove(&channel_id) {
                                if let Err(resp) = self.inner.behaviour_mut().shard_xfer
                                    .send_response(channel, response)
                                {
                                    warn!(cid = %resp.cid, "failed to send chunk response — channel closed");
                                }
                            } else {
                                warn!(channel_id, "no pending channel for chunk response");
                            }
                        }
                        Some(SwarmCommand::Shutdown) | None => {
                            info!("swarm event loop shutting down");
                            return Ok(());
                        }
                    }
                }

                _ = reaper_interval.tick() => {
                    self.reap_orphaned_channels();
                }
            }
        }
    }

    /// Remove pending response channels that have been waiting longer than
    /// [`PENDING_CHANNEL_TIMEOUT`]. Dropping the `ResponseChannel` causes
    /// libp2p to signal a timeout to the requester.
    fn reap_orphaned_channels(&mut self) {
        let reaped = reap_stale_entries(
            &mut self.pending_shard_channels,
            PENDING_CHANNEL_TIMEOUT,
        );
        if reaped > 0 {
            info!(reaped, remaining = self.pending_shard_channels.len(), "orphaned channel cleanup");
        }
    }

    // ── Private event dispatcher ──────────────────────────────────────────────

    fn handle_swarm_event(
        &mut self,
        event:    SwarmEvent<LocalMeshBehaviourEvent>,
        event_tx: &mpsc::Sender<SumNetEvent>,
    ) {
        match event {
            // ── mDNS ──────────────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Mdns(e)) => {
                discovery::handle_mdns_event(
                    e,
                    &mut self.inner.behaviour_mut().gossipsub,
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
                let data  = message.data;
                info!(
                    from  = %propagation_source,
                    %topic,
                    bytes = data.len(),
                    "gossipsub message received"
                );
                if let Err(e) = event_tx.try_send(SumNetEvent::MessageReceived {
                    from:  propagation_source,
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
                identify::Event::Received { peer_id, info, .. }
            )) => {
                debug!(%peer_id, "identify received");
                if let Some(l1_addr) =
                    crate::identity::l1_address_from_peer_public_key(&info.public_key)
                {
                    if let Err(e) = event_tx.try_send(SumNetEvent::PeerIdentified {
                        peer_id,
                        l1_address: l1_addr,
                    }) {
                        warn!(%e, "event channel full — dropping PeerIdentified");
                    }
                }

                // Feed the observed-address hint into our external-address
                // candidates. AutoNAT needs this to settle on Public, and
                // DCUtR needs it to advertise a dialable candidate during
                // hole-punch coordination.
                self.inner.add_external_address(info.observed_addr.clone());

                // Detect relay-capable peers so we can request a reservation
                // when AutoNAT determines we are Private. The relay hop
                // protocol string contains "relay" — matching works across
                // minor version bumps.
                let supports_relay = info
                    .protocols
                    .iter()
                    .any(|p| p.as_ref().contains("relay"));
                if supports_relay && !self.relay_peers.contains(&peer_id) {
                    self.relay_peers.push(peer_id);
                    debug!(%peer_id, "identified as relay-capable peer");
                }

                // Feed identified peer's listen addresses into Kademlia
                // so the DHT routing table populates beyond bootstrap nodes.
                for addr in &info.listen_addrs {
                    self.inner.behaviour_mut().kademlia
                        .add_address(&peer_id, addr.clone());
                }
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Identify(e)) => {
                debug!(?e, "identify event");
            }

            // ── Kademlia DHT ─────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Kademlia(
                kad::Event::RoutingUpdated { peer, addresses, .. }
            )) => {
                // Wire Kademlia-discovered peers into gossipsub (same as mDNS).
                self.inner.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                let addrs: Vec<Multiaddr> = addresses.iter().cloned().collect();
                info!(%peer, addr_count = addrs.len(), "Kademlia peer discovered");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerDiscovered {
                    peer_id: peer,
                    addrs,
                }) {
                    warn!(%e, "event channel full — dropping PeerDiscovered (kad)");
                }
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Kademlia(
                kad::Event::OutboundQueryProgressed { result, .. }
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
                nat::handle_relay_client_event(e, event_tx);
            }
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Dcutr(e)) => {
                nat::handle_dcutr_event(e, event_tx);
            }

            // ── Chunk transfer ────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXfer(
                request_response::Event::Message { peer, message, .. }
            )) => {
                match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let channel_id = self.next_channel_id;
                        self.next_channel_id += 1;
                        self.pending_shard_channels.insert(channel_id, (channel, Instant::now()));
                        info!(
                            %peer,
                            cid = %request.cid,
                            channel_id,
                            "inbound chunk request"
                        );
                        if let Err(e) = event_tx.try_send(SumNetEvent::ShardRequested {
                            peer_id: peer,
                            request,
                            channel_id,
                        }) {
                            // Event was dropped — the app layer will never
                            // respond, so remove the orphaned channel immediately.
                            self.pending_shard_channels.remove(&channel_id);
                            warn!(%e, channel_id, "event channel full — dropping ShardRequested and cleaning up pending channel");
                        }
                    }
                    request_response::Message::Response { response, .. } => {
                        info!(
                            %peer,
                            cid = %response.cid,
                            offset = response.offset,
                            bytes = response.data.len(),
                            "chunk data received"
                        );
                        if let Err(e) = event_tx.try_send(SumNetEvent::ShardReceived {
                            peer_id: peer,
                            response,
                        }) {
                            warn!(%e, "event channel full — dropping ShardReceived");
                        }
                    }
                }
            }

            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXfer(
                request_response::Event::OutboundFailure { peer, error, .. }
            )) => {
                warn!(%peer, %error, "chunk request outbound failure");
                if let Err(e) = event_tx.try_send(SumNetEvent::ShardRequestFailed {
                    peer_id: peer,
                    error: error.to_string(),
                }) {
                    warn!(%e, "event channel full — dropping ShardRequestFailed");
                }
            }

            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXfer(
                request_response::Event::InboundFailure { peer, error, .. }
            )) => {
                debug!(%peer, %error, "chunk request inbound failure");
            }

            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXfer(
                request_response::Event::ResponseSent { peer, .. }
            )) => {
                debug!(%peer, "chunk response sent");
            }

            // ── Transport ─────────────────────────────────────────────────────
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(%address, "listening on address");
                if let Err(e) = event_tx.try_send(SumNetEvent::Listening { addr: address }) {
                    warn!(%e, "event channel full — dropping Listening");
                }
            }

            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                info!(%peer_id, "connection established");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerConnected { peer_id }) {
                    warn!(%e, "event channel full — dropping PeerConnected");
                }
            }

            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                debug!(%peer_id, ?cause, "connection closed");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerDisconnected { peer_id }) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // A trivial stand-in for ResponseChannel (which we can't construct).
    // reap_stale_entries is generic over V, so any type works.

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
}
