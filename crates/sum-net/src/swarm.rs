use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    gossipsub, identify, mdns,
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
};

// ── SwarmCommand ──────────────────────────────────────────────────────────────

/// Commands sent from the [`crate::SumNet`] handle into the running swarm loop.
#[derive(Debug)]
pub enum SwarmCommand {
    /// Publish bytes to a named Gossipsub topic.
    Publish { topic: String, data: Vec<u8> },

    /// Send a chunk request to a remote peer.
    RequestShard { peer_id: PeerId, request: ShardRequest },

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

    /// Stores response channels received from inbound chunk requests.
    pending_shard_channels: HashMap<u64, ResponseChannel<ShardResponse>>,

    /// Monotonic counter for channel IDs.
    next_channel_id: u64,
}

impl SumSwarm {
    /// Construct and configure the Swarm with an externally-provided keypair.
    ///
    /// The keypair should be derived from the user's SUM Chain L1 wallet seed
    /// via [`crate::identity::keypair_from_seed`].
    ///
    /// Transport:  QUIC (TLS 1.3 baked-in)
    /// Behaviour:  mDNS + Gossipsub + Identify + chunk transfer
    /// Listener:   `0.0.0.0:<config.listen_port>` (0 = OS-assigned)
    pub fn build(config: &NetConfig, keypair: Keypair) -> Result<Self> {
        let gossip_cfg = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(10))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .history_length(10)
            .history_gossip(3)
            .build()
            .map_err(|msg| anyhow::anyhow!("gossipsub config error: {msg}"))?;

        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_quic()
            .with_behaviour(|key| {
                let mdns = mdns::tokio::Behaviour::new(
                    mdns::Config::default(),
                    key.public().to_peer_id(),
                )?;

                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg,
                )
                .map_err(|msg| anyhow::anyhow!("gossipsub init: {msg}"))?;

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/sum-node/0.1.0".into(),
                    key.public(),
                ));

                let shard_xfer = request_response::Behaviour::new(
                    [(SHARD_XFER_PROTOCOL.to_string(), ProtocolSupport::Full)],
                    request_response::Config::default()
                        .with_request_timeout(Duration::from_secs(120)),
                );

                Ok(LocalMeshBehaviour { mdns, gossipsub, identify, shard_xfer })
            })?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        let listen_addr: Multiaddr =
            format!("/ip4/0.0.0.0/udp/{}/quic-v1", config.listen_port)
                .parse()
                .context("invalid QUIC listen multiaddr")?;

        swarm
            .listen_on(listen_addr)
            .context("failed to bind QUIC listener")?;

        Ok(Self {
            inner:  swarm,
            gossip: GossipManager::new(),
            pending_shard_channels: HashMap::new(),
            next_channel_id: 0,
        })
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
                        Some(SwarmCommand::SendShardResponse { channel_id, response }) => {
                            if let Some(channel) = self.pending_shard_channels.remove(&channel_id) {
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
            }
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
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::Identify(e)) => {
                debug!(?e, "identify event");
            }

            // ── Chunk transfer ────────────────────────────────────────────────
            SwarmEvent::Behaviour(LocalMeshBehaviourEvent::ShardXfer(
                request_response::Event::Message { peer, message, .. }
            )) => {
                match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let channel_id = self.next_channel_id;
                        self.next_channel_id += 1;
                        self.pending_shard_channels.insert(channel_id, channel);
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
                            warn!(%e, "event channel full — dropping ShardRequested");
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
