use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use libp2p::{gossipsub, kad, mdns, PeerId, StreamProtocol};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::events::SumNetEvent;

/// Kademlia protocol identifier for the SUM Storage Node DHT.
const KAD_PROTOCOL: &str = "/sum/kad/1.0.0";

/// Build the Kademlia behaviour with SUM-specific tuning.
///
/// - Server mode: every SUM node is a full DHT participant.
/// - 20-way replication: aggressive for a young network.
/// - 1-hour record TTL with 10-minute republish keeps the DHT fresh.
pub fn build_kademlia(local_peer_id: PeerId) -> kad::Behaviour<kad::store::MemoryStore> {
    let store = kad::store::MemoryStore::new(local_peer_id);

    let mut config = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
    config
        .set_query_timeout(Duration::from_secs(60))
        .set_record_ttl(Some(Duration::from_secs(3600)))
        .set_replication_factor(NonZeroUsize::new(20).unwrap())
        .set_publication_interval(Some(Duration::from_secs(600)))
        .set_provider_record_ttl(Some(Duration::from_secs(3600)));

    let mut behaviour = kad::Behaviour::with_config(local_peer_id, store, config);
    behaviour.set_mode(Some(kad::Mode::Server));
    behaviour
}

/// Handle a single mDNS event.
///
/// Wires newly-discovered peers into Gossipsub (via `add_explicit_peer`) and
/// emits the appropriate [`SumNetEvent`] for each distinct PeerId.
/// Called directly from the swarm event loop to keep that loop thin.
pub fn handle_mdns_event(
    event: mdns::Event,
    gossipsub: &mut gossipsub::Behaviour,
    event_tx: &mpsc::Sender<SumNetEvent>,
) {
    match event {
        mdns::Event::Discovered(list) => {
            // Group all addresses by PeerId — a peer may be reachable on
            // multiple addresses, but we emit one event per peer.
            let mut by_peer: HashMap<PeerId, Vec<_>> = HashMap::new();
            for (peer_id, addr) in list {
                gossipsub.add_explicit_peer(&peer_id);
                by_peer.entry(peer_id).or_default().push(addr);
            }
            for (peer_id, addrs) in by_peer {
                info!(%peer_id, addr_count = addrs.len(), "mDNS discovered peer");
                if let Err(e) =
                    event_tx.try_send(SumNetEvent::PeerDiscovered { peer_id, addrs })
                {
                    warn!(%e, "event channel full — dropping PeerDiscovered");
                }
            }
        }

        mdns::Event::Expired(list) => {
            // Deduplicate: one peer may appear once per expired address.
            let mut expired: HashSet<PeerId> = HashSet::new();
            for (peer_id, _addr) in list {
                expired.insert(peer_id);
            }
            for peer_id in expired {
                gossipsub.remove_explicit_peer(&peer_id);
                debug!(%peer_id, "mDNS peer expired");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerExpired { peer_id }) {
                    warn!(%e, "event channel full — dropping PeerExpired");
                }
            }
        }
    }
}
