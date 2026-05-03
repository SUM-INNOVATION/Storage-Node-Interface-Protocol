use libp2p::{Multiaddr, PeerId};

use crate::codec::{ShardRequest, ShardRequestV2, ShardResponse, ShardResponseV2};

/// Domain-level events emitted by the SUM Storage Node networking layer.
/// Never exposes raw libp2p internals to callers.
#[derive(Debug, Clone)]
pub enum SumNetEvent {
    /// The local node is now listening on a new address.
    Listening { addr: Multiaddr },

    /// A new peer was discovered via mDNS on the local network.
    PeerDiscovered {
        peer_id: PeerId,
        addrs: Vec<Multiaddr>,
    },

    /// A previously discovered mDNS peer is no longer visible.
    PeerExpired { peer_id: PeerId },

    /// A transport-layer connection was established.
    PeerConnected { peer_id: PeerId },

    /// A transport-layer connection was closed.
    PeerDisconnected { peer_id: PeerId },

    /// A Gossipsub message was received.
    MessageReceived {
        from: PeerId,
        topic: String,
        data: Vec<u8>,
    },

    /// A remote peer requested a V1 chunk from us (`/sum/storage/v1`).
    /// The higher layer (sum-store) should call
    /// `SumNet::respond_shard(channel_id, response)` with a V1 response.
    ShardRequested {
        peer_id: PeerId,
        request: ShardRequest,
        channel_id: u64,
    },

    /// We received V1 chunk data from a remote peer (response to our V1 request).
    ShardReceived {
        peer_id: PeerId,
        response: ShardResponse,
    },

    /// A remote peer issued a V2 request to us (`/sum/storage/v2`). V2
    /// covers four variants: `Pull`, `Push`, `ManifestPush`,
    /// `ManifestPull` (chain plan v3.2 §3.6 receive-side). The higher
    /// layer (sum-node V2 dispatcher) routes to `PushValidator` /
    /// manifest store / serve, and replies with
    /// `SumNet::respond_shard_v2(channel_id, ShardResponseV2)`.
    ShardRequestedV2 {
        peer_id: PeerId,
        request: ShardRequestV2,
        channel_id: u64,
    },

    /// We received a V2 response from a remote peer (response to our V2 request).
    ShardReceivedV2 {
        peer_id: PeerId,
        response: ShardResponseV2,
    },

    /// An outbound chunk request failed (V1 OR V2 — covers both since
    /// the libp2p outbound failure is protocol-agnostic).
    ShardRequestFailed {
        peer_id: PeerId,
        error: String,
    },

    /// A peer's L1 address was identified via the libp2p identify protocol.
    /// Used by the ACL checker to map PeerId -> L1 Address.
    PeerIdentified {
        peer_id: PeerId,
        l1_address: [u8; 20],
    },

    /// AutoNAT determined whether this node is publicly reachable.
    NatStatusChanged {
        is_public: bool,
        public_addr: Option<Multiaddr>,
    },

    /// A relay reservation was established — this node is now reachable
    /// via a `/p2p-circuit` address at the listed relay peer.
    RelayReservation {
        relay_peer_id: PeerId,
        relay_addr: Multiaddr,
    },

    /// DCUtR upgraded a relay circuit to a direct QUIC connection
    /// via UDP hole-punching.
    HolePunchSucceeded { peer_id: PeerId },

    /// DCUtR hole-punch failed — the relay circuit remains the data path.
    HolePunchFailed {
        peer_id: PeerId,
        error: String,
    },
}
