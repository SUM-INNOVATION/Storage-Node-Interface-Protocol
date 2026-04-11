//! Single source of truth for mutating the `PeerId → L1 address` identity map
//! in response to network events.
//!
//! Every event loop that maintains a `peer_addresses` map (`run_listen`,
//! `run_ingest`, `DownloadOrchestrator`) must route events through
//! [`apply_peer_event`] so the bookkeeping logic lives in one place.

use std::collections::HashMap;

use sum_net::{PeerId, SumNetEvent};

/// Outcome of applying a single network event to the peer identity map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerMapChange {
    /// A new or updated mapping was inserted.
    Inserted,
    /// An existing mapping was removed.
    Removed,
    /// The map was not modified (event is irrelevant or peer was unknown).
    Unchanged,
}

/// Apply the peer-identity side-effects of `event` to `map`.
///
/// Only `PeerIdentified` (insert) and `PeerDisconnected` (remove) mutate the
/// map.  All other variants — including `PeerExpired` — are no-ops because an
/// mDNS TTL expiry does not imply the transport connection has dropped.
pub fn apply_peer_event(
    map: &mut HashMap<PeerId, [u8; 20]>,
    event: &SumNetEvent,
) -> PeerMapChange {
    match event {
        SumNetEvent::PeerIdentified { peer_id, l1_address } => {
            map.insert(*peer_id, *l1_address);
            PeerMapChange::Inserted
        }
        SumNetEvent::PeerDisconnected { peer_id } => {
            if map.remove(peer_id).is_some() {
                PeerMapChange::Removed
            } else {
                PeerMapChange::Unchanged
            }
        }
        _ => PeerMapChange::Unchanged,
    }
}
