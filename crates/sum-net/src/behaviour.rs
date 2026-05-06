use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, relay, request_response,
    swarm::NetworkBehaviour,
};

use crate::codec::VersionedShardCodec;

/// Composed [`NetworkBehaviour`] for the SUM Storage Node mesh.
///
/// The `#[derive(NetworkBehaviour)]` macro generates `LocalMeshBehaviourEvent`
/// with variants matching each field name in PascalCase:
/// - `Mdns(mdns::Event)`
/// - `Gossipsub(gossipsub::Event)`
/// - `Identify(identify::Event)`
/// - `ShardXfer(request_response::Event<ShardRequestVersioned, ShardResponseVersioned>)`
///   — the codec dispatches V1 vs V2 on the libp2p-negotiated protocol.
///   See [`crate::codec::VersionedShardCodec`].
/// - `Kademlia(kad::Event)`
/// - `Autonat(autonat::Event)`
/// - `Relay(relay::Event)`
/// - `RelayClient(relay::client::Event)`
/// - `Dcutr(dcutr::Event)`
#[derive(NetworkBehaviour)]
pub struct LocalMeshBehaviour {
    pub mdns: mdns::tokio::Behaviour,
    pub gossipsub: gossipsub::Behaviour,
    pub identify: identify::Behaviour,
    pub shard_xfer: request_response::Behaviour<VersionedShardCodec>,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    pub autonat: autonat::Behaviour,
    pub relay: relay::Behaviour,
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
}
