use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use libp2p::{PeerId, StreamProtocol, gossipsub, kad, mdns};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::events::SumNetEvent;
use crate::policy::{NetPolicy, PolicySource};

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
///
/// Under an outbound allow-list, discovered addresses the policy refuses are
/// dropped (logged and counted under `mdns`), and a peer with no allowed
/// address is neither wired into gossipsub nor reported. Without one, every
/// discovered address is kept, as before.
pub fn handle_mdns_event(
    event: mdns::Event,
    gossipsub: &mut gossipsub::Behaviour,
    policy: &NetPolicy,
    event_tx: &mpsc::Sender<SumNetEvent>,
) {
    match event {
        mdns::Event::Discovered(list) => {
            // Group all addresses by PeerId — a peer may be reachable on
            // multiple addresses, but we emit one event per peer.
            let mut by_peer: HashMap<PeerId, Vec<_>> = HashMap::new();
            for (peer_id, addr) in list {
                if !policy.admit(PolicySource::Mdns, &addr, Some(peer_id)) {
                    continue;
                }
                gossipsub.add_explicit_peer(&peer_id);
                by_peer.entry(peer_id).or_default().push(addr);
            }
            for (peer_id, addrs) in by_peer {
                info!(%peer_id, addr_count = addrs.len(), "mDNS discovered peer");
                if let Err(e) = event_tx.try_send(SumNetEvent::PeerDiscovered { peer_id, addrs }) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::AddrPolicy;
    use libp2p::Multiaddr;

    fn ma(s: &str) -> Multiaddr {
        s.parse().expect("test multiaddr must parse")
    }

    fn gossipsub() -> gossipsub::Behaviour {
        gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(libp2p::identity::Keypair::generate_ed25519()),
            gossipsub::Config::default(),
        )
        .expect("gossipsub")
    }

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    fn drain(rx: &mut mpsc::Receiver<SumNetEvent>) -> Vec<SumNetEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// mDNS discoveries outside the allow-list are refused, counted under
    /// `mdns`, and never reach the upper layer; a peer with no allowed address
    /// is not reported at all. A peer with one allowed address is reported
    /// with that address only.
    #[test]
    fn mdns_discoveries_outside_the_allow_list_are_refused_and_counted() {
        let policy = NetPolicy::new(
            true,
            AddrPolicy::parse(["/ip4/192.0.2.10/udp/4001/quic-v1"]).unwrap(),
        );
        let (tx, mut rx) = mpsc::channel(16);
        let mut gs = gossipsub();
        let lan_only = peer();
        let mixed = peer();

        handle_mdns_event(
            mdns::Event::Discovered(vec![
                (lan_only, ma("/ip4/192.0.2.20/udp/4001/quic-v1")),
                (lan_only, ma("/ip4/192.0.2.20/tcp/4001")),
                (mixed, ma("/ip4/192.0.2.10/udp/4001/quic-v1")),
                (mixed, ma("/ip4/192.0.2.10/udp/4002/quic-v1")),
            ]),
            &mut gs,
            &policy,
            &tx,
        );

        assert_eq!(policy.refusals().mdns, 3);
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            SumNetEvent::PeerDiscovered { peer_id, addrs } => {
                assert_eq!(*peer_id, mixed);
                assert_eq!(addrs, &vec![ma("/ip4/192.0.2.10/udp/4001/quic-v1")]);
            }
            other => panic!("expected PeerDiscovered, got {other:?}"),
        }
    }

    /// Without a policy every discovery is reported, as before.
    #[test]
    fn mdns_discoveries_pass_unchanged_without_a_policy() {
        let policy = NetPolicy::new(true, None);
        let (tx, mut rx) = mpsc::channel(16);
        let mut gs = gossipsub();
        let p = peer();
        handle_mdns_event(
            mdns::Event::Discovered(vec![
                (p, ma("/ip4/192.0.2.20/udp/4001/quic-v1")),
                (p, ma("/ip4/192.0.2.20/tcp/4001")),
            ]),
            &mut gs,
            &policy,
            &tx,
        );
        assert_eq!(policy.refusals().total(), 0);
        match drain(&mut rx).as_slice() {
            [SumNetEvent::PeerDiscovered { peer_id, addrs }] => {
                assert_eq!(*peer_id, p);
                assert_eq!(addrs.len(), 2);
            }
            other => panic!("expected one PeerDiscovered, got {other:?}"),
        }
    }
}
