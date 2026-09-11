// ── Module declarations ───────────────────────────────────────────────────────

pub mod behaviour;
pub mod capability; // deferred — WAN capability advertisement protocol
pub mod codec;
pub mod correlation;
pub mod discovery;
pub mod events;
pub mod gossip;
pub mod identity;
pub mod nat; // deferred — AutoNAT / DCUtR / relay
pub mod swarm;
pub mod transport; // deferred — TCP/Noise fallback transport

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use behaviour::{
    LocalMeshBehaviour, SHARD_XFER_REQUEST_TIMEOUT, build_shard_xfer_v1, build_shard_xfer_v2,
    shard_xfer_config, shard_xfer_v1_protocols, shard_xfer_v2_protocols,
};
pub use codec::{
    SHARD_XFER_PROTOCOL, SHARD_XFER_PROTOCOL_V1, SHARD_XFER_PROTOCOL_V2, ShardCodec, ShardRequest,
    ShardRequestV2, ShardRequestVersioned, ShardResponse, ShardResponseV2, ShardResponseVersioned,
    VersionedShardCodec,
};
pub use correlation::{
    CorrelationError, MANIFEST_CID_PREFIX, OutboundKey, OutboundOrigin, OutboundRequestKind,
    PeerMismatch, RecordCollision, RecordError, RequestDomain,
};
pub use events::SumNetEvent;
pub use gossip::{TOPIC_CAPABILITY, TOPIC_STORAGE, TOPIC_TEST};
pub use identity::{
    keypair_from_seed, l1_address_base58, l1_address_from_base58, l1_address_from_keypair,
    l1_address_from_peer_public_key, peer_id_from_keypair,
};
pub use libp2p::PeerId;
pub use libp2p::identity::Keypair;

// ── Imports ───────────────────────────────────────────────────────────────────

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
    cmd_tx: mpsc::Sender<SwarmCommand>,
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<SwarmCommand>(CHANNEL_CAPACITY);

        tokio::spawn(async move {
            if let Err(e) = sum_swarm.run(event_tx, cmd_rx).await {
                tracing::error!(%e, "swarm event loop exited with error");
            }
        });

        Ok(Self {
            cmd_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
        })
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

    /// Pull a chunk from a remote peer over V1.
    ///
    /// Routed to the V1-only behaviour, so it negotiates `/sum/storage/v1`
    /// against a V1-only peer and against a dual-protocol peer alike. This is
    /// the path that keeps legacy peers readable.
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
                request: ShardRequest {
                    cid,
                    offset,
                    max_bytes,
                    push_data: None,
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot request chunk"))
    }

    /// Request a file's DataManifest from a peer over V1.
    ///
    /// Uses the `"manifest:<hex_root>"` convention within the
    /// `/sum/storage/v1` protocol, and routes to the V1-only behaviour so a
    /// V1-only peer can answer it.
    pub async fn request_manifest(&self, peer_id: PeerId, merkle_root_hex: String) -> Result<()> {
        let cid = format!("{MANIFEST_CID_PREFIX}{merkle_root_hex}");
        self.cmd_tx
            .send(SwarmCommand::RequestShard {
                peer_id,
                request: ShardRequest {
                    cid,
                    offset: None,
                    max_bytes: None,
                    push_data: None,
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot request manifest"))
    }

    // ── No V1 push ──────────────────────────────────────────────────────
    //
    // `push_chunk` and `push_chunk_shared` were here. They are gone, along
    // with `SwarmCommand::PushShard`, and nothing replaced them at this layer.
    //
    // V1 push was a `ShardRequest` with `push_data: Some(bytes)` — no Merkle
    // proof, no assignment check, no signature. The receiving side verified the
    // CID against the bytes and stored them, which proves only that the sender
    // hashed what it sent. Any peer could fill any archive.
    //
    // The replacement is [`Self::push_chunk_v2`] / [`Self::push_manifest_v2`],
    // which carry the proof the receiver validates. There is deliberately **no
    // fallback path** from V2 push to V1 push: a fallback would mean a peer
    // that declines the authenticated protocol gets the unauthenticated one,
    // which is the whole defect restored on demand. A V2 push to a peer that
    // does not speak `/sum/storage/v2` fails, and failing is the point.
    //
    // Inbound V1 pushes are refused by `sum-node`'s inbound dispatcher before
    // any lock is taken; see `sum_node::shard_dispatch`.

    /// Send a V1 chunk response on a pending response channel.
    ///
    /// Refused by the swarm if the channel arrived on the V2 behaviour; both
    /// behaviours yield the same Rust channel type, so provenance is tracked
    /// explicitly. See `swarm::PendingChannel`.
    pub async fn respond_shard(&self, channel_id: u64, response: ShardResponse) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::SendShardResponse {
                channel_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot respond chunk"))
    }

    // ── V2 chunk transfer (chain plan v3.2 §3.6) ─────────────────────────
    //
    // Each helper carries the precise on-wire shape — no `Option<>`
    // squeezing like V1 — so call sites can't miss a field.
    //
    // Every helper below routes to the V2-only behaviour, which advertises
    // `/sum/storage/v2` and nothing else. Against a peer that speaks V2 the
    // negotiation has exactly one candidate; against a peer that does not, the
    // substream fails to negotiate and the call surfaces as an
    // `OutboundFailure`. There is **no automatic V2 → V1 fallback**, and no V1
    // push helper to fall back to — see the note above.

    /// V2 Pull — request `[offset, offset+max_bytes)` of the chunk at `cid`.
    pub async fn pull_chunk_v2(
        &self,
        peer_id: PeerId,
        cid: String,
        offset: u64,
        max_bytes: u64,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::RequestShardV2 {
                peer_id,
                request: ShardRequestV2::Pull {
                    cid,
                    offset,
                    max_bytes,
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot pull chunk v2"))
    }

    /// V2 Push — send a chunk and its strict Merkle proof. The receiver
    /// runs `PushValidator::validate_push` (chain plan v3.2 §3.6
    /// receive-side) before persisting.
    pub async fn push_chunk_v2(
        &self,
        peer_id: PeerId,
        data: Vec<u8>,
        merkle_root: [u8; 32],
        chunk_index: u32,
        merkle_path: Vec<[u8; 32]>,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::RequestShardV2 {
                peer_id,
                request: ShardRequestV2::Push {
                    data,
                    merkle_root,
                    chunk_index,
                    merkle_path,
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot push chunk v2"))
    }

    /// V2 ManifestPush — send the CBOR manifest blob keyed by `merkle_root`.
    /// The receiver validates internal consistency (root recomputed from
    /// the manifest's chunk descriptors) before persisting.
    pub async fn push_manifest_v2(
        &self,
        peer_id: PeerId,
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
    ) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::RequestShardV2 {
                peer_id,
                request: ShardRequestV2::ManifestPush {
                    merkle_root,
                    manifest_bytes,
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot push manifest v2"))
    }

    /// V2 ManifestPull — request the CBOR manifest for `merkle_root`.
    pub async fn pull_manifest_v2(&self, peer_id: PeerId, merkle_root: [u8; 32]) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::RequestShardV2 {
                peer_id,
                request: ShardRequestV2::ManifestPull { merkle_root },
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot pull manifest v2"))
    }

    /// Send a V2 response on a pending response channel.
    ///
    /// Refused by the swarm if the channel arrived on the V1 behaviour.
    pub async fn respond_shard_v2(&self, channel_id: u64, response: ShardResponseV2) -> Result<()> {
        self.cmd_tx
            .send(SwarmCommand::SendShardResponseV2 {
                channel_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("swarm task has stopped — cannot respond v2"))
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
