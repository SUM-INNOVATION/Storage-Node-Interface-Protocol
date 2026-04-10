//! Integration tests for the `peer_addresses` identity map.
//!
//! These tests import and exercise the **production** [`apply_peer_event`]
//! function from `sum_node::peer_state` — the same function called by
//! `main.rs` (`run_listen`, `run_ingest`) and `download.rs`.

use std::collections::HashMap;

use sum_net::{Keypair, PeerId, SumNetEvent};
use sum_net::{l1_address_from_keypair, peer_id_from_keypair};
use sum_node::peer_state::{apply_peer_event, PeerMapChange};

/// Generate a unique (PeerId, L1 address) pair from a fresh Ed25519 keypair.
fn random_peer() -> (PeerId, [u8; 20]) {
    let kp = Keypair::generate_ed25519();
    let pid = peer_id_from_keypair(&kp);
    let addr = l1_address_from_keypair(&kp);
    (pid, addr)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn peer_identified_inserts_into_map() {
    let mut map = HashMap::new();
    let (pid, addr) = random_peer();

    let change = apply_peer_event(
        &mut map,
        &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr },
    );
    assert_eq!(change, PeerMapChange::Inserted);
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&pid), Some(&addr));
}

#[test]
fn peer_disconnected_removes_from_map() {
    let mut map = HashMap::new();
    let (pid, addr) = random_peer();

    apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr });
    assert_eq!(map.len(), 1);

    let change = apply_peer_event(&mut map, &SumNetEvent::PeerDisconnected { peer_id: pid });
    assert_eq!(change, PeerMapChange::Removed);
    assert!(map.is_empty());
}

#[test]
fn peer_expired_does_not_remove_from_map() {
    let mut map = HashMap::new();
    let (pid, addr) = random_peer();

    apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr });
    assert_eq!(map.len(), 1);

    // PeerExpired must NOT remove the entry.
    let change = apply_peer_event(&mut map, &SumNetEvent::PeerExpired { peer_id: pid });
    assert_eq!(change, PeerMapChange::Unchanged);
    assert_eq!(map.len(), 1, "PeerExpired must not remove the identity mapping");
    assert_eq!(map.get(&pid), Some(&addr));
}

#[test]
fn rapid_churn_map_stays_bounded() {
    let mut map = HashMap::new();

    for _ in 0..200 {
        let (pid, addr) = random_peer();
        apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr });
        apply_peer_event(&mut map, &SumNetEvent::PeerDisconnected { peer_id: pid });
    }
    assert!(map.is_empty(), "map must be empty after all peers disconnect");
}

#[test]
fn interleaved_churn_map_bounded() {
    let mut map = HashMap::new();

    // Phase 1: 100 peers join.
    let peers: Vec<_> = (0..100).map(|_| random_peer()).collect();
    for &(pid, addr) in &peers {
        apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr });
    }
    assert_eq!(map.len(), 100);

    // Phase 2: all 100 expire (mDNS TTL) — map must NOT shrink.
    for &(pid, _) in &peers {
        apply_peer_event(&mut map, &SumNetEvent::PeerExpired { peer_id: pid });
    }
    assert_eq!(map.len(), 100, "PeerExpired must not affect map size");

    // Phase 3: all 100 disconnect — now the map must drain.
    for &(pid, _) in &peers {
        apply_peer_event(&mut map, &SumNetEvent::PeerDisconnected { peer_id: pid });
    }
    assert!(map.is_empty(), "map must be empty after all peers disconnect");
}

#[test]
fn disconnect_unknown_peer_is_noop() {
    let mut map = HashMap::new();
    let (pid, _) = random_peer();

    let change = apply_peer_event(&mut map, &SumNetEvent::PeerDisconnected { peer_id: pid });
    assert_eq!(change, PeerMapChange::Unchanged);
    assert!(map.is_empty());
}

#[test]
fn re_identify_updates_address() {
    let mut map = HashMap::new();
    let (pid, addr1) = random_peer();
    let addr2 = [0xBB; 20];

    apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr1 });
    assert_eq!(map.get(&pid), Some(&addr1));

    apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr2 });
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&pid), Some(&addr2));
}

#[test]
fn sustained_churn_with_mixed_events() {
    let mut map = HashMap::new();
    let mut high_water = 0usize;

    for _ in 0..500 {
        let (pid, addr) = random_peer();
        apply_peer_event(&mut map, &SumNetEvent::PeerIdentified { peer_id: pid, l1_address: addr });

        if map.len() > high_water {
            high_water = map.len();
        }

        // Expire does nothing.
        apply_peer_event(&mut map, &SumNetEvent::PeerExpired { peer_id: pid });
        // Disconnect removes.
        apply_peer_event(&mut map, &SumNetEvent::PeerDisconnected { peer_id: pid });
    }

    assert!(map.is_empty());
    assert_eq!(high_water, 1, "high-water mark should be 1 in serial churn");
}
