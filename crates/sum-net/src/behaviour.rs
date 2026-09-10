use std::time::Duration;

use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, relay,
    request_response::{self, ProtocolSupport},
    swarm::NetworkBehaviour,
};

use crate::codec::{SHARD_XFER_PROTOCOL_V1, SHARD_XFER_PROTOCOL_V2, VersionedShardCodec};

/// Request timeout applied to both shard-transfer behaviours.
///
/// Exported because [`crate::correlation::POISON_TTL`] is reasoned about
/// relative to it, and because the loopback regression tests build behaviours
/// that must be configured exactly as production configures them.
pub const SHARD_XFER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The protocol list of the V1 behaviour: `/sum/storage/v1` and nothing else.
///
/// The single-element-ness is the point. `libp2p-request-response`'s
/// `send_request` attaches **every** protocol the behaviour was registered
/// with to **every** outbound request (0.28.0 `lib.rs`), so a behaviour that
/// carries two protocols cannot be asked to speak one of them. Choosing a
/// protocol per request is therefore a choice of *behaviour*, and this list is
/// where that choice is made real.
pub fn shard_xfer_v1_protocols() -> [(String, ProtocolSupport); 1] {
    [(SHARD_XFER_PROTOCOL_V1.to_string(), ProtocolSupport::Full)]
}

/// The protocol list of the V2 behaviour: `/sum/storage/v2` and nothing else.
/// See [`shard_xfer_v1_protocols`] for why it is exactly one entry.
pub fn shard_xfer_v2_protocols() -> [(String, ProtocolSupport); 1] {
    [(SHARD_XFER_PROTOCOL_V2.to_string(), ProtocolSupport::Full)]
}

/// The request-response config both shard behaviours use.
pub fn shard_xfer_config() -> request_response::Config {
    request_response::Config::default().with_request_timeout(SHARD_XFER_REQUEST_TIMEOUT)
}

/// Build the `/sum/storage/v1`-only shard behaviour.
pub fn build_shard_xfer_v1() -> request_response::Behaviour<VersionedShardCodec> {
    request_response::Behaviour::with_codec(
        VersionedShardCodec::default(),
        shard_xfer_v1_protocols(),
        shard_xfer_config(),
    )
}

/// Build the `/sum/storage/v2`-only shard behaviour.
pub fn build_shard_xfer_v2() -> request_response::Behaviour<VersionedShardCodec> {
    request_response::Behaviour::with_codec(
        VersionedShardCodec::default(),
        shard_xfer_v2_protocols(),
        shard_xfer_config(),
    )
}

/// Composed [`NetworkBehaviour`] for the SUM Storage Node mesh.
///
/// The `#[derive(NetworkBehaviour)]` macro generates `LocalMeshBehaviourEvent`
/// with variants matching each field name in PascalCase:
/// - `Mdns(mdns::Event)`
/// - `Gossipsub(gossipsub::Event)`
/// - `Identify(identify::Event)`
/// - `ShardXferV1(request_response::Event<ShardRequestVersioned, ShardResponseVersioned>)`
/// - `ShardXferV2(request_response::Event<ShardRequestVersioned, ShardResponseVersioned>)`
/// - `Kademlia(kad::Event)`
/// - `Autonat(autonat::Event)`
/// - `Relay(relay::Event)`
/// - `RelayClient(relay::client::Event)`
/// - `Dcutr(dcutr::Event)`
///
/// ## Why shard transfer is two behaviours and not one
///
/// One behaviour registering both protocols is what this node used to do, and
/// it makes per-request protocol selection impossible: `send_request` attaches
/// the behaviour's whole protocol list to every request and lets multistream
/// pick the first mutually-supported entry. A V1-shaped request sent to a
/// dual-protocol peer therefore negotiated `/sum/storage/v2` and the codec had
/// to refuse to write it — a V1 pull to a peer that speaks V2 could not be
/// expressed at all.
///
/// Two behaviours, each with exactly one protocol, make the version a routing
/// decision at the call site: [`crate::swarm::SwarmCommand::RequestShard`]
/// goes to `shard_xfer_v1`, every V2 command goes to `shard_xfer_v2`, and
/// negotiation has one candidate either way.
///
/// The cost is that `OutboundRequestId` is unique only per behaviour, so both
/// counters start at zero and collide immediately. That is why every outbound
/// record is keyed by [`crate::correlation::OutboundKey`] — the id paired with
/// the [`crate::correlation::RequestDomain`] naming which of these two fields
/// minted it — and never by the raw id.
#[derive(NetworkBehaviour)]
pub struct LocalMeshBehaviour {
    pub mdns: mdns::tokio::Behaviour,
    pub gossipsub: gossipsub::Behaviour,
    pub identify: identify::Behaviour,
    /// `/sum/storage/v1` only. Inbound V1 pulls are served; inbound V1 pushes
    /// are refused by `sum-node`'s inbound dispatcher. No outbound V1 push
    /// command exists any more.
    pub shard_xfer_v1: request_response::Behaviour<VersionedShardCodec>,
    /// `/sum/storage/v2` only. Every V2 operation is routed here explicitly.
    pub shard_xfer_v2: request_response::Behaviour<VersionedShardCodec>,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    pub autonat: autonat::Behaviour,
    pub relay: relay::Behaviour,
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each behaviour advertises exactly one protocol. Collapsing them back
    /// into a single dual-protocol behaviour is the defect this whole split
    /// exists to prevent, and it shows up here first.
    #[test]
    fn each_shard_behaviour_advertises_exactly_one_protocol() {
        let v1 = shard_xfer_v1_protocols();
        let v2 = shard_xfer_v2_protocols();

        assert_eq!(v1.len(), 1, "the V1 behaviour must carry one protocol");
        assert_eq!(v2.len(), 1, "the V2 behaviour must carry one protocol");
        assert_eq!(v1[0].0, SHARD_XFER_PROTOCOL_V1);
        assert_eq!(v2[0].0, SHARD_XFER_PROTOCOL_V2);
        assert!(
            v1.iter().all(|(p, _)| p != SHARD_XFER_PROTOCOL_V2),
            "the V1 behaviour must not advertise V2 — send_request would attach it"
        );
        assert!(
            v2.iter().all(|(p, _)| p != SHARD_XFER_PROTOCOL_V1),
            "the V2 behaviour must not advertise V1 — send_request would attach it"
        );
    }
}
