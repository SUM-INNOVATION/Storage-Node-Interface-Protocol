// ── Module declarations ───────────────────────────────────────────────────────

pub mod behaviour;
pub mod capability;  // deferred — WAN capability advertisement protocol
pub mod codec;
pub mod discovery;
pub mod events;
pub mod gossip;
pub mod identity;
pub mod nat;         // deferred — AutoNAT / DCUtR / relay
pub mod swarm;
pub mod transport;   // deferred — TCP/Noise fallback transport

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use events::SumNetEvent;
pub use gossip::{TOPIC_CAPABILITY, TOPIC_STORAGE, TOPIC_TEST};
pub use codec::{ShardCodec, ShardRequest, ShardResponse, SHARD_XFER_PROTOCOL};
pub use identity::{keypair_from_seed, l1_address_from_keypair, peer_id_from_keypair};
pub use libp2p::identity::Keypair;

// ── Imports ───────────────────────────────────────────────────────────────────

use anyhow::Result;
use libp2p::PeerId;
use tokio::sync::mpsc;

use sum_types::config::NetConfig;

use crate::swarm::{SumSwarm, SwarmCommand};

/// Internal channel buffer.
const CHANNEL_CAPACITY: usize = 256;

// ── SumNet ───────────────────────────────────────────────────────────────────

/// Top-level handle to the SUM Storage Node P2P networking layer.
///
/// Owns two async channels that communicate with a background `tokio` task
/// running the [`swarm::SumSwarm`] event loop.
pub struct SumNet {
    cmd_tx:   mpsc::Sender<SwarmCommand>,
    event_rx: mpsc::Receiver<SumNetEvent>,
}

impl SumNet {
    /// Build the swarm with the given keypair, subscribe to all topics,
    /// and spawn the event loop task.
    ///
    /// The `keypair` should be derived from the user's L1 wallet seed via
    /// [`identity::keypair_from_seed`].
    pub async fn new(config: NetConfig, keypair: Keypair) -> Result<Self> {
        let mut sum_swarm = SumSwarm::build(&config, keypair)?;
        sum_swarm.subscribe_all_topics()?;

        let (event_tx, event_rx) = mpsc::channel::<SumNetEvent>(CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx)     = mpsc::channel::<SwarmCommand>(CHANNEL_CAPACITY);

        tokio::spawn(async move {
            if let Err(e) = sum_swarm.run(event_tx, cmd_rx).await {
                tracing::error!(%e, "swarm event loop exited with error");
            }
        });

        Ok(Self { cmd_tx, event_rx })
    }

    // ── Gossipsub ──────────────────────────────────────────────────────

    /// Publish `data` to the named Gossipsub topic.
    pub async fn publish(&self, topic: &str, data: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::Publish {
                topic: topic.to_string(),
                data,
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot publish"))
    }

    // ── Chunk transfer ────────────────────────────────────────────────

    /// Request a chunk from a remote peer.
    pub async fn request_shard_chunk(
        &self,
        peer_id: PeerId,
        cid: String,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::RequestShard {
                peer_id,
                request: ShardRequest { cid, offset, max_bytes },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot request chunk"))
    }

    /// Send a chunk response on a pending response channel.
    pub async fn respond_shard(
        &self,
        channel_id: u64,
        response: ShardResponse,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::SendShardResponse { channel_id, response })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot respond chunk"))
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    /// Receive the next event from the mesh.
    pub async fn next_event(&mut self) -> Option<SumNetEvent> {
        self.event_rx.recv().await
    }

    /// Signal the swarm loop to shut down gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::Shutdown)
            .await
            .map_err(|_| anyhow::anyhow!("swarm task already stopped"))
    }
}
