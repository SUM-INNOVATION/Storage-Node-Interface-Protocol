use libp2p::{gossipsub, identify, kad, mdns, request_response, swarm::NetworkBehaviour};

use crate::codec::ShardCodec;

/// Composed [`NetworkBehaviour`] for the SUM Storage Node mesh.
///
/// The `#[derive(NetworkBehaviour)]` macro generates `LocalMeshBehaviourEvent`
/// with variants matching each field name in PascalCase:
/// - `Mdns(mdns::Event)`
/// - `Gossipsub(gossipsub::Event)`
/// - `Identify(identify::Event)`
/// - `ShardXfer(request_response::Event<ShardRequest, ShardResponse>)`
/// - `Kademlia(kad::Event)`
#[derive(NetworkBehaviour)]
pub struct LocalMeshBehaviour {
    pub mdns:       mdns::tokio::Behaviour,
    pub gossipsub:  gossipsub::Behaviour,
    pub identify:   identify::Behaviour,
    pub shard_xfer: request_response::Behaviour<ShardCodec>,
    pub kademlia:   kad::Behaviour<kad::store::MemoryStore>,
}
