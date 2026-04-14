use libp2p::{Multiaddr, PeerId};

use crate::codec::{ShardRequest, ShardResponse};

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

    /// A remote peer requested a chunk from us.
    /// The higher layer (sum-store) should call
    /// `SumNet::respond_shard(channel_id, response)`.
    ShardRequested {
        peer_id: PeerId,
        request: ShardRequest,
        channel_id: u64,
    },

    /// We received chunk data from a remote peer (response to our request).
    ShardReceived {
        peer_id: PeerId,
        response: ShardResponse,
    },

    /// An outbound chunk request failed.
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
