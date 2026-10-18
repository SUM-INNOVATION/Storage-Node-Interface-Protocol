//! Completion condition for V2 ingest peer discovery.
//!
//! Before this module, `ingest-v2` and `resume` ended discovery on a timing
//! guess: once the first archive identified itself they slept 500 ms, drained
//! events until 200 ms passed with none, and pushed to whoever had been seen.
//! An archive that identified later was never pushed to, so its chunks stayed
//! under-replicated and the file stayed `Pending`.
//!
//! Discovery now ends on an explicit condition instead:
//!
//! - **Complete** when every *required* archive has at least one identified,
//!   still-connected peer. The required set is computed from the chain: the
//!   distinct archives the V2 assignment gives the chunks being pushed (see
//!   [`required_archives`]).
//! - **Timed out** at a fixed deadline, reporting which required archives
//!   were never identified.
//!
//! Archives are counted by chain address, never by libp2p `PeerId`: two peer
//! ids reporting the same address count once, and a peer that disconnects
//! stops counting. Peers whose address is not required (validators, other
//! clients, archives the assignment does not use) never count.
//!
//! [`ArchiveDiscovery`] is pure. It is fed identify and disconnect events plus
//! the current time, does no I/O, and is tested without a network. The async
//! driver that reads events from the swarm lives in `ingest_v2`.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use sum_net::{PeerId, SumNetEvent};
use sum_store::assignment_v2::assigned_archives;
use tokio::time::Instant;

use crate::peer_state::{PeerMapChange, apply_peer_event};

/// Where discovery stands at a given instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryStatus {
    /// Every required archive has a live identified peer.
    Complete,
    /// Some required archives are not yet identified and the deadline has
    /// not passed.
    Waiting { missing: usize },
    /// The deadline passed with these required archives still unidentified.
    TimedOut { missing: Vec<[u8; 20]> },
}

/// Why discovery ended without completing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryFailure {
    /// The deadline passed first.
    TimedOut {
        required: usize,
        missing: Vec<[u8; 20]>,
        waited: std::time::Duration,
    },
    /// The network event stream ended (swarm shut down) first.
    NetworkClosed {
        required: usize,
        missing: Vec<[u8; 20]>,
    },
}

impl DiscoveryFailure {
    /// Required archives that were never identified.
    pub fn missing(&self) -> &[[u8; 20]] {
        match self {
            Self::TimedOut { missing, .. } | Self::NetworkClosed { missing, .. } => missing,
        }
    }
}

fn fmt_addrs(addrs: &[[u8; 20]]) -> String {
    addrs
        .iter()
        .map(sum_net::l1_address_base58)
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for DiscoveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut {
                required,
                missing,
                waited,
            } => write!(
                f,
                "peer discovery timed out after {waited:?}: {} of {required} required \
                 archives were not identified on the network (counted by chain address): {}",
                missing.len(),
                fmt_addrs(missing),
            ),
            Self::NetworkClosed { required, missing } => write!(
                f,
                "peer discovery stopped because the network shut down: {} of {required} \
                 required archives were not identified (counted by chain address): {}",
                missing.len(),
                fmt_addrs(missing),
            ),
        }
    }
}

impl std::error::Error for DiscoveryFailure {}

/// Pure discovery state: the required archive set, the deadline, and the
/// live `PeerId → address` identities seen so far.
#[derive(Debug, Clone)]
pub struct ArchiveDiscovery {
    required: BTreeSet<[u8; 20]>,
    deadline: Instant,
    live: HashMap<PeerId, [u8; 20]>,
}

impl ArchiveDiscovery {
    pub fn new(required: BTreeSet<[u8; 20]>, deadline: Instant) -> Self {
        Self {
            required,
            deadline,
            live: HashMap::new(),
        }
    }

    /// Record a peer already known to be identified and connected (for
    /// example, one identified before this discovery round started).
    pub fn seed(&mut self, peer: PeerId, address: [u8; 20]) {
        self.live.insert(peer, address);
    }

    /// Apply one network event. Only `PeerIdentified` and
    /// `PeerDisconnected` change the state, with exactly the semantics of
    /// [`apply_peer_event`]. Returns whether the state changed.
    pub fn observe(&mut self, event: &SumNetEvent) -> bool {
        apply_peer_event(&mut self.live, event) != PeerMapChange::Unchanged
    }

    pub fn required_len(&self) -> usize {
        self.required.len()
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Required archives with no live identified peer, in address order.
    pub fn missing(&self) -> Vec<[u8; 20]> {
        let identified: BTreeSet<&[u8; 20]> = self.live.values().collect();
        self.required
            .iter()
            .filter(|a| !identified.contains(a))
            .copied()
            .collect()
    }

    /// Completion is checked before the deadline, so an archive identified
    /// at the deadline itself still counts.
    pub fn status(&self, now: Instant) -> DiscoveryStatus {
        let missing = self.missing();
        if missing.is_empty() {
            DiscoveryStatus::Complete
        } else if now >= self.deadline {
            DiscoveryStatus::TimedOut { missing }
        } else {
            DiscoveryStatus::Waiting {
                missing: missing.len(),
            }
        }
    }
}

/// The distinct archives the V2 assignment gives `chunks` of the file
/// `merkle_root` under `snapshot` with replication factor `r`.
///
/// This is the set the push wave sends to, so it is the set discovery has to
/// reach. When `r` covers the whole snapshot every archive is assigned every
/// chunk, and the loop is skipped.
pub fn required_archives(
    merkle_root: &[u8; 32],
    snapshot: &[[u8; 20]],
    chunks: impl IntoIterator<Item = u32>,
    r: u32,
) -> BTreeSet<[u8; 20]> {
    let all: BTreeSet<[u8; 20]> = snapshot.iter().copied().collect();
    let mut chunks = chunks.into_iter().peekable();
    if chunks.peek().is_none() || r == 0 {
        return BTreeSet::new();
    }
    if r as usize >= all.len() {
        return all;
    }
    let mut required = BTreeSet::new();
    for chunk_index in chunks {
        required.extend(assigned_archives(merkle_root, snapshot, chunk_index, r));
        if required.len() == all.len() {
            break;
        }
    }
    required
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn addr(b: u8) -> [u8; 20] {
        [b; 20]
    }

    fn peer() -> PeerId {
        sum_net::Keypair::generate_ed25519().public().to_peer_id()
    }

    fn identified(peer_id: PeerId, l1_address: [u8; 20]) -> SumNetEvent {
        SumNetEvent::PeerIdentified {
            peer_id,
            l1_address,
        }
    }

    fn disconnected(peer_id: PeerId) -> SumNetEvent {
        SumNetEvent::PeerDisconnected { peer_id }
    }

    fn three() -> BTreeSet<[u8; 20]> {
        [addr(1), addr(2), addr(3)].into_iter().collect()
    }

    #[test]
    fn delayed_identification_completes_whenever_the_last_archive_arrives() {
        let t0 = Instant::now();
        let mut d = ArchiveDiscovery::new(three(), t0 + Duration::from_secs(30));
        d.observe(&identified(peer(), addr(1)));
        assert_eq!(d.status(t0), DiscoveryStatus::Waiting { missing: 2 });
        d.observe(&identified(peer(), addr(2)));
        // Long after the old 0.7 s window: still waiting, not finished.
        assert_eq!(
            d.status(t0 + Duration::from_secs(5)),
            DiscoveryStatus::Waiting { missing: 1 }
        );
        d.observe(&identified(peer(), addr(3)));
        assert_eq!(
            d.status(t0 + Duration::from_secs(20)),
            DiscoveryStatus::Complete
        );
    }

    #[test]
    fn insufficient_archives_time_out_naming_the_missing_ones() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(30);
        let mut d = ArchiveDiscovery::new(three(), deadline);
        d.observe(&identified(peer(), addr(1)));
        d.observe(&identified(peer(), addr(3)));
        assert_eq!(
            d.status(deadline - Duration::from_millis(1)),
            DiscoveryStatus::Waiting { missing: 1 }
        );
        assert_eq!(
            d.status(deadline),
            DiscoveryStatus::TimedOut {
                missing: vec![addr(2)]
            }
        );
    }

    #[test]
    fn completion_at_the_deadline_counts() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(1);
        let mut d = ArchiveDiscovery::new(three(), deadline);
        for a in three() {
            d.observe(&identified(peer(), a));
        }
        assert_eq!(d.status(deadline), DiscoveryStatus::Complete);
        assert_eq!(
            d.status(deadline + Duration::from_secs(1)),
            DiscoveryStatus::Complete
        );
    }

    #[test]
    fn a_disconnect_after_identify_stops_counting() {
        let t0 = Instant::now();
        let mut d = ArchiveDiscovery::new(three(), t0 + Duration::from_secs(30));
        let p3 = peer();
        d.observe(&identified(peer(), addr(1)));
        d.observe(&identified(peer(), addr(2)));
        d.observe(&identified(p3, addr(3)));
        assert_eq!(d.status(t0), DiscoveryStatus::Complete);
        assert!(d.observe(&disconnected(p3)));
        assert_eq!(d.status(t0), DiscoveryStatus::Waiting { missing: 1 });
        assert_eq!(d.missing(), vec![addr(3)]);
        // Reconnecting and identifying again counts again.
        d.observe(&identified(p3, addr(3)));
        assert_eq!(d.status(t0), DiscoveryStatus::Complete);
    }

    #[test]
    fn the_same_peer_identified_twice_counts_once() {
        let t0 = Instant::now();
        let mut d = ArchiveDiscovery::new(three(), t0 + Duration::from_secs(30));
        let p1 = peer();
        d.observe(&identified(p1, addr(1)));
        d.observe(&identified(p1, addr(1)));
        d.observe(&identified(peer(), addr(2)));
        assert_eq!(d.status(t0), DiscoveryStatus::Waiting { missing: 1 });
        // One disconnect removes it: there was only ever one identity.
        d.observe(&disconnected(p1));
        assert_eq!(d.missing(), vec![addr(1), addr(3)]);
    }

    #[test]
    fn two_peer_ids_for_one_archive_count_as_one_archive() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(30);
        let mut d = ArchiveDiscovery::new(three(), deadline);
        let (pa, pb) = (peer(), peer());
        d.observe(&identified(pa, addr(1)));
        d.observe(&identified(pb, addr(1)));
        d.observe(&identified(peer(), addr(2)));
        // Three identified peers, two distinct required archives.
        assert_eq!(
            d.status(deadline),
            DiscoveryStatus::TimedOut {
                missing: vec![addr(3)]
            }
        );
        d.observe(&identified(peer(), addr(3)));
        assert_eq!(d.status(t0), DiscoveryStatus::Complete);
        // Losing one of the two peers for archive 1 leaves it reachable.
        d.observe(&disconnected(pa));
        assert_eq!(d.status(t0), DiscoveryStatus::Complete);
        d.observe(&disconnected(pb));
        assert_eq!(d.missing(), vec![addr(1)]);
    }

    #[test]
    fn peers_outside_the_required_set_never_count() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(30);
        let mut d = ArchiveDiscovery::new(three(), deadline);
        for b in 10..20 {
            d.observe(&identified(peer(), addr(b)));
        }
        d.observe(&identified(peer(), addr(1)));
        assert_eq!(
            d.status(deadline),
            DiscoveryStatus::TimedOut {
                missing: vec![addr(2), addr(3)]
            }
        );
    }

    #[test]
    fn other_events_change_nothing() {
        let t0 = Instant::now();
        let mut d = ArchiveDiscovery::new(three(), t0 + Duration::from_secs(30));
        let p = peer();
        d.observe(&identified(p, addr(1)));
        assert!(!d.observe(&SumNetEvent::PeerExpired { peer_id: p }));
        assert!(!d.observe(&SumNetEvent::PeerDiscovered {
            peer_id: p,
            addrs: vec![],
        }));
        assert!(!d.observe(&disconnected(peer())));
        assert_eq!(d.missing(), vec![addr(2), addr(3)]);
    }

    #[test]
    fn seeded_peers_count_and_an_empty_requirement_is_complete() {
        let t0 = Instant::now();
        let mut d = ArchiveDiscovery::new(three(), t0);
        for a in three() {
            d.seed(peer(), a);
        }
        assert_eq!(d.status(t0), DiscoveryStatus::Complete);
        let empty = ArchiveDiscovery::new(BTreeSet::new(), t0);
        assert_eq!(empty.status(t0), DiscoveryStatus::Complete);
    }

    #[test]
    fn failure_messages_name_the_missing_count_and_addresses() {
        let f = DiscoveryFailure::TimedOut {
            required: 3,
            missing: vec![addr(2)],
            waited: Duration::from_secs(30),
        };
        let s = f.to_string();
        assert!(s.contains("1 of 3 required archives"), "{s}");
        assert!(s.contains(&sum_net::l1_address_base58(&addr(2))), "{s}");
        let c = DiscoveryFailure::NetworkClosed {
            required: 3,
            missing: vec![addr(1), addr(2)],
        };
        assert!(c.to_string().contains("2 of 3 required archives"));
        assert_eq!(c.missing(), &[addr(1), addr(2)]);
    }

    fn snapshot(n: u8) -> Vec<[u8; 20]> {
        (0..n).map(|i| addr(0xA0 + i)).collect()
    }

    #[test]
    fn required_archives_is_the_union_of_the_chunk_assignments() {
        let root = [7u8; 32];
        let snap = snapshot(8);
        let mut expected = BTreeSet::new();
        for i in 0..2 {
            expected.extend(assigned_archives(&root, &snap, i, 3));
        }
        assert_eq!(required_archives(&root, &snap, 0..2, 3), expected);
        // Only the chunks asked for (resume pushes a subset).
        let only_one: BTreeSet<_> = assigned_archives(&root, &snap, 1, 3).into_iter().collect();
        assert_eq!(required_archives(&root, &snap, [1], 3), only_one);
    }

    #[test]
    fn required_archives_covers_the_whole_snapshot_when_r_does() {
        let root = [9u8; 32];
        let snap = snapshot(3);
        let all: BTreeSet<_> = snap.iter().copied().collect();
        assert_eq!(required_archives(&root, &snap, 0..1, 3), all);
        assert_eq!(required_archives(&root, &snap, 0..1, 5), all);
        assert!(required_archives(&root, &snap, 0..0, 3).is_empty());
        assert!(required_archives(&root, &snap, 0..4, 0).is_empty());
    }

    #[test]
    fn required_archives_on_many_chunks_reaches_every_assigned_archive() {
        let root = [3u8; 32];
        let snap = snapshot(6);
        let req = required_archives(&root, &snap, 0..64, 3);
        let mut union = BTreeSet::new();
        for i in 0..64 {
            union.extend(assigned_archives(&root, &snap, i, 3));
        }
        assert_eq!(req, union);
    }
}
