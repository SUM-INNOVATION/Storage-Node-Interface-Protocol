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
pub use identity::{
    keypair_from_seed, l1_address_from_keypair, l1_address_base58,
    l1_address_from_base58, l1_address_from_peer_public_key, peer_id_from_keypair,
};
pub use libp2p::identity::Keypair;
pub use libp2p::PeerId;

// ── Imports ───────────────────────────────────────────────────────────────────

use std::sync::Arc;

use anyhow::Result;
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
    event_rx: tokio::sync::Mutex<mpsc::Receiver<SumNetEvent>>,
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

        // Bootstrap Kademlia DHT when WAN mode is enabled.
        if config.enable_wan {
            sum_swarm.bootstrap_kademlia(&config.bootstrap_peers)?;
        }

        let (event_tx, event_rx) = mpsc::channel::<SumNetEvent>(CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx)     = mpsc::channel::<SwarmCommand>(CHANNEL_CAPACITY);

        tokio::spawn(async move {
            if let Err(e) = sum_swarm.run(event_tx, cmd_rx).await {
                tracing::error!(%e, "swarm event loop exited with error");
            }
        });

        Ok(Self { cmd_tx, event_rx: tokio::sync::Mutex::new(event_rx) })
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
                request: ShardRequest { cid, offset, max_bytes, push_data: None },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot request chunk"))
    }

    /// Request a file's DataManifest from a peer.
    ///
    /// Uses the `"manifest:<hex_root>"` convention within the existing
    /// `/sum/storage/v1` protocol.
    pub async fn request_manifest(
        &self,
        peer_id: PeerId,
        merkle_root_hex: String,
    ) -> Result<()> {
        let cid = format!("manifest:{merkle_root_hex}");
        self.cmd_tx
            .send(SwarmCommand::RequestShard {
                peer_id,
                request: ShardRequest { cid, offset: None, max_bytes: None, push_data: None },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot request manifest"))
    }

    /// Push a chunk to a remote peer for storage.
    ///
    /// The peer will verify the CID, store the chunk, and respond with an ACK.
    ///
    /// This method takes an owned `Vec<u8>` for backwards compatibility.
    /// New callers fanning a single chunk to multiple replicas should
    /// prefer [`Self::push_chunk_shared`], which avoids per-replica copies.
    pub async fn push_chunk(
        &self,
        peer_id: PeerId,
        cid: String,
        data: Vec<u8>,
    ) -> Result<()> {
        let shared: Arc<[u8]> = Arc::from(data.into_boxed_slice());
        self.push_chunk_shared(peer_id, cid, shared).await
    }

    /// Push a chunk using a shared, ref-counted buffer.
    ///
    /// Multiple replica pushes for the same chunk can clone the same
    /// `Arc<[u8]>` cheaply (pointer bump only). The full `Vec<u8>`
    /// materialization happens once, inside the swarm command handler,
    /// immediately before libp2p serializes the request — so buffered
    /// commands in the channel hold pointer-sized handles instead of
    /// full chunk copies.
    pub async fn push_chunk_shared(
        &self,
        peer_id: PeerId,
        cid: String,
        data: Arc<[u8]>,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::PushShard { peer_id, cid, data })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot push chunk"))
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
    pub async fn next_event(&self) -> Option<SumNetEvent> {
        self.event_rx.lock().await.recv().await
    }

    /// Signal the swarm loop to shut down gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::Shutdown)
            .await
            .map_err(|_| anyhow::anyhow!("swarm task already stopped"))
    }
}
