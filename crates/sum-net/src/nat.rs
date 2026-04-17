//! NAT traversal layer: AutoNAT probing, Circuit Relay v2, and DCUtR hole-punching.
//!
//! ## Flow
//!
//! 1. **AutoNAT** asks random DHT peers "can you dial me back?" to determine
//!    whether this node has a public address or is behind NAT.
//! 2. If **Private**, the node picks a relay peer and listens on a
//!    `/p2p-circuit` address so firewalled peers can reach it.
//! 3. **DCUtR** automatically upgrades relay circuits to direct QUIC
//!    connections via simultaneous UDP hole-punching when both ends support it.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::Duration;

use libp2p::{autonat, dcutr, multiaddr::Protocol, relay, Multiaddr, PeerId, Swarm};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::behaviour::LocalMeshBehaviour;
use crate::events::SumNetEvent;

// ── NAT status tracking ──────────────────────────────────────────────────────

/// Simplified NAT status for the swarm to reason about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatStatus {
    Unknown,
    Public(Multiaddr),
    Private,
}

// ── Relay candidate + reservation state machine ─────────────────────────────

/// A peer that might be usable as a Circuit Relay v2 server.
///
/// Candidates are seeded from two sources:
/// - `--bootstrap-peer` addresses (seeded with `confirmed: false` — we have
///   the address but don't yet know if the peer supports the relay hop
///   protocol).
/// - `identify::Event::Received` — when the remote advertises the relay hop
///   protocol, we set `confirmed = true` and merge their reported
///   `listen_addrs` with the existing base addresses.
///
/// Only `confirmed == true` candidates are eligible to receive reservation
/// requests — this prevents sending reservation requests to bootstrap peers
/// that aren't actually relay servers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayCandidate {
    /// WAN-dialable base addresses (no `/p2p-circuit`, no LAN addresses).
    pub addrs: Vec<Multiaddr>,
    /// `true` only after identify confirms the peer advertises the relay
    /// hop protocol. Do not set this from bootstrap or other speculative
    /// paths.
    pub confirmed: bool,
}

/// Tri-state relay reservation machine.
///
/// ```text
///     None ──mark_pending──▶ Pending(peer) ──mark_active──▶ Active(peer)
///      ▲                        │                                │
///      │                        │                                │
///      └─────── reset ──────────┴────────── reset ──────────────┘
/// ```
///
/// - `None`: no reservation in flight; the AutoNAT handler is free to pick
///   a new relay candidate.
/// - `Pending(peer)`: we issued `swarm.listen_on(...)` for `peer`'s circuit
///   but haven't yet received a `relay::client::Event::ReservationReqAccepted`.
/// - `Active(peer)`: the relay accepted the reservation and we're reachable
///   via `/p2p/<peer>/p2p-circuit`.
///
/// All failure paths — denial, listener close, timeout, transport error —
/// must call [`RelayReservationState::reset`] so the next AutoNAT cycle can
/// retry. Leaving the state in `Pending` or `Active` after a failure wedges
/// the node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelayReservationState {
    #[default]
    None,
    Pending(PeerId),
    Active(PeerId),
}

impl RelayReservationState {
    /// `true` while a reservation is either in flight or already active.
    /// AutoNAT's Private branch consults this to avoid stacking requests.
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Pending(_) | Self::Active(_))
    }

    /// Return the peer this state references, if any.
    pub fn peer(&self) -> Option<PeerId> {
        match self {
            Self::None => None,
            Self::Pending(p) | Self::Active(p) => Some(*p),
        }
    }

    /// Transition: we issued a circuit listen_on against `peer`.
    ///
    /// Unconditional — the caller is responsible for having checked
    /// [`Self::is_busy`] first.
    pub fn mark_pending(&mut self, peer: PeerId) {
        *self = Self::Pending(peer);
    }

    /// Transition: the relay accepted our reservation.
    ///
    /// Only upgrades `Pending(p)` → `Active(p)` when the reservation was
    /// for the same peer. Stray `Accepted` events (e.g. for a peer we
    /// never marked pending, or after `reset`) are ignored silently.
    pub fn mark_active(&mut self, peer: PeerId) {
        if matches!(self, Self::Pending(p) if *p == peer) {
            *self = Self::Active(peer);
        }
    }

    /// Transition: failure of any kind — denied, closed, timed out.
    /// Resets to `None` so the next AutoNAT cycle is free to retry.
    pub fn reset(&mut self) {
        *self = Self::None;
    }
}

// ── AutoNAT construction ─────────────────────────────────────────────────────

/// Build the AutoNAT behaviour.
///
/// Probing cadence: every 60 s retry, 300 s refresh once confirmed.
/// Confidence threshold: 3 confirmations before declaring Public/Private.
pub fn build_autonat(local_peer_id: PeerId) -> autonat::Behaviour {
    let config = autonat::Config {
        retry_interval: Duration::from_secs(60),
        refresh_interval: Duration::from_secs(300),
        confidence_max: 3,
        throttle_server_period: Duration::from_secs(15),
        ..Default::default()
    };
    autonat::Behaviour::new(local_peer_id, config)
}

// ── Relay server construction ────────────────────────────────────────────────

/// Build the Circuit Relay v2 server behaviour.
///
/// When `enabled` is true, the node accepts relay reservations from firewalled
/// peers and relays short-lived circuits so DCUtR can upgrade them.
///
/// Circuit bytes capped at 8 MiB — relay circuits are only meant to last long
/// enough for DCUtR to hole-punch (seconds, not minutes). Allowing bulk
/// traffic through the relay would DDoS volunteer nodes before the direct
/// connection upgrade succeeds.
///
/// Rate limits prevent a single peer from exhausting relay resources:
/// - Max 2 reservations per peer per 60 s
/// - Max 4 circuit opens per peer per 60 s
///
/// When `enabled` is false, `max_reservations: 0` means no clients are
/// accepted — the behaviour is inert but present in the NetworkBehaviour struct.
pub fn build_relay_server(local_peer_id: PeerId, enabled: bool) -> relay::Behaviour {
    let config = if enabled {
        relay::Config {
            max_reservations: 128,
            max_reservations_per_peer: 4,
            max_circuit_duration: Duration::from_secs(120),
            max_circuit_bytes: 1 << 23, // 8 MiB — tight cap for pre-upgrade relay
            ..Default::default()
        }
        // Rate-limit reservation requests: max 2 per peer per 60 s.
        .reservation_rate_per_peer(NonZeroU32::new(2).unwrap(), Duration::from_secs(60))
        // Rate-limit circuit opens: max 4 per source peer per 60 s.
        .circuit_src_per_peer(NonZeroU32::new(4).unwrap(), Duration::from_secs(60))
        // Rate-limit circuit opens per IP: prevents a single IP from DDoSing
        // the relay by spinning up multiple peer IDs.
        .circuit_src_per_ip(NonZeroU32::new(8).unwrap(), Duration::from_secs(60))
    } else {
        relay::Config {
            max_reservations: 0,
            ..Default::default()
        }
    };
    relay::Behaviour::new(local_peer_id, config)
}

// ── AutoNAT event handler ────────────────────────────────────────────────────

/// Process an AutoNAT event. When we transition to Private, attempt to
/// acquire a relay reservation from a **confirmed** relay candidate.
///
/// `relay_peers` maps each candidate peer to a [`RelayCandidate`]; only
/// entries with `confirmed == true` (i.e., identify-confirmed relay hop
/// support) are eligible as reservation targets. Bootstrap-only entries
/// that haven't been confirmed are skipped.
///
/// `reservation_state` is the tri-state [`RelayReservationState`] machine.
/// The Private branch issues a new reservation only when the state is
/// `None` — `Pending` and `Active` are treated as busy.
pub fn handle_autonat_event(
    event: autonat::Event,
    relay_peers: &HashMap<PeerId, RelayCandidate>,
    swarm: &mut Swarm<LocalMeshBehaviour>,
    nat_status: &mut NatStatus,
    reservation_state: &mut RelayReservationState,
    event_tx: &mpsc::Sender<SumNetEvent>,
) {
    match event {
        autonat::Event::StatusChanged { old, new } => {
            info!(?old, ?new, "AutoNAT status changed");

            match new {
                autonat::NatStatus::Public(addr) => {
                    *nat_status = NatStatus::Public(addr.clone());
                    // We're public — clear any active/pending reservation.
                    reservation_state.reset();
                    let _ = event_tx.try_send(SumNetEvent::NatStatusChanged {
                        is_public: true,
                        public_addr: Some(addr),
                    });
                }

                autonat::NatStatus::Private => {
                    *nat_status = NatStatus::Private;
                    let _ = event_tx.try_send(SumNetEvent::NatStatusChanged {
                        is_public: false,
                        public_addr: None,
                    });

                    // Already holding (or awaiting) a reservation? Don't
                    // stack a second one on top.
                    if reservation_state.is_busy() {
                        debug!(
                            state = ?reservation_state,
                            "NAT still private — reservation already in flight, skipping"
                        );
                        return;
                    }

                    // Only identify-confirmed relay candidates with at
                    // least one known WAN address are eligible. This is
                    // the key guard against blindly reserving on a
                    // bootstrap peer that isn't actually a relay.
                    let candidate = relay_peers
                        .iter()
                        .find(|(_, c)| c.confirmed && !c.addrs.is_empty());

                    if let Some((relay_peer, candidate)) = candidate {
                        if request_relay_reservation(swarm, *relay_peer, &candidate.addrs) {
                            reservation_state.mark_pending(*relay_peer);
                        }
                    } else {
                        warn!(
                            "NAT is private but no confirmed relay peers with WAN addresses \
                             known — unreachable over WAN"
                        );
                    }
                }

                autonat::NatStatus::Unknown => {
                    *nat_status = NatStatus::Unknown;
                }
            }
        }

        other => {
            debug!(?other, "autonat event");
        }
    }
}

/// Ask the swarm to listen on a relay circuit address.
///
/// Builds a full dialable circuit multiaddr from each of the relay's known
/// WAN base addresses (e.g. `/ip4/1.2.3.4/tcp/4001` →
/// `/ip4/1.2.3.4/tcp/4001/p2p/<relay>/p2p-circuit`) and passes it to
/// `swarm.listen_on`. The relay-client transport needs the base address to
/// resolve the reservation target; passing a bare `/p2p/<relay>/p2p-circuit`
/// fails silently because libp2p cannot look up the relay's dialable
/// endpoints from the transport's own address book.
///
/// Returns `true` on the first successful listen_on call, `false` if every
/// base was rejected. Logs both Display and Debug of the error on failure
/// since `TransportError`'s Display can render empty for some variants.
fn request_relay_reservation(
    swarm: &mut Swarm<LocalMeshBehaviour>,
    relay_peer: PeerId,
    relay_bases: &[Multiaddr],
) -> bool {
    if relay_bases.is_empty() {
        warn!(%relay_peer, "no known dialable addresses for relay — cannot reserve");
        return false;
    }

    for base in relay_bases {
        let circuit_addr = build_circuit_listen_addr(base, relay_peer);
        match swarm.listen_on(circuit_addr.clone()) {
            Ok(_) => {
                info!(%relay_peer, %circuit_addr, "requesting relay reservation");
                return true;
            }
            Err(e) => {
                warn!(
                    %relay_peer,
                    %circuit_addr,
                    error = %e,
                    error_debug = ?e,
                    "listen_on(circuit) failed — trying next base",
                );
            }
        }
    }

    warn!(%relay_peer, "all known relay bases rejected circuit listen_on");
    false
}

/// Construct a `/p2p-circuit` listen multiaddr from a relay's base dialable
/// address.
///
/// Handles all four input shapes:
///
/// - **Bare base** (no `/p2p/`):
///   `/ip4/1.2.3.4/tcp/4001` → `/ip4/1.2.3.4/tcp/4001/p2p/<relay>/p2p-circuit`
/// - **DNS base**:
///   `/dns4/relay.example.com/tcp/4001` → `/dns4/relay.example.com/tcp/4001/p2p/<relay>/p2p-circuit`
/// - **Base with `/p2p/<relay>` already appended** (matching peer id):
///   `/ip4/1.2.3.4/tcp/4001/p2p/<relay>` → `/ip4/1.2.3.4/tcp/4001/p2p/<relay>/p2p-circuit`
/// - **Base with a mismatched `/p2p/<other>` segment**: the wrong peer id is
///   stripped and replaced with `relay_peer`. This prevents silently
///   reserving on the wrong peer when the caller's bookkeeping has a
///   stale/mismatched address.
/// - **Already ending in `/p2p-circuit`**: returned unchanged (idempotent).
pub(crate) fn build_circuit_listen_addr(
    base: &Multiaddr,
    relay_peer: PeerId,
) -> Multiaddr {
    // Idempotent: already a circuit address? Return unchanged.
    if base.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
        return base.clone();
    }
    // Strip any existing /p2p/<...> segment from the base and re-append
    // /p2p/<relay_peer> ourselves. This is safe when the base already
    // carries the right peer id (result is identical), and it corrects
    // the address when the base carries a mismatched id.
    let mut result = Multiaddr::empty();
    for proto in base.iter() {
        if !matches!(proto, Protocol::P2p(_)) {
            result.push(proto);
        }
    }
    result.push(Protocol::P2p(relay_peer));
    result.push(Protocol::P2pCircuit);
    result
}

// ── Relay server event handler ───────────────────────────────────────────────

/// Process relay server events. Mostly logging — the relay behaviour is
/// self-contained and doesn't need external state.
pub fn handle_relay_server_event(event: relay::Event) {
    match event {
        relay::Event::ReservationReqAccepted {
            src_peer_id,
            renewed,
            ..
        } => {
            info!(
                %src_peer_id,
                renewed,
                "relay reservation accepted for remote peer"
            );
        }
        relay::Event::ReservationReqDenied { src_peer_id, .. } => {
            debug!(%src_peer_id, "relay reservation denied (limit reached)");
        }
        relay::Event::CircuitReqDenied {
            src_peer_id,
            dst_peer_id,
            ..
        } => {
            debug!(%src_peer_id, %dst_peer_id, "relay circuit denied");
        }
        other => {
            debug!(?other, "relay server event");
        }
    }
}

// ── Relay client event handler ───────────────────────────────────────────────

/// Process relay client events.
///
/// - `ReservationReqAccepted` → upgrade the reservation state from
///   `Pending(peer)` to `Active(peer)` and emit `RelayReservation` upstream.
/// - Any other event is currently treated as advisory. Listener-close /
///   denial semantics are handled in `swarm.rs`'s `ListenerClosed` arm by
///   resetting `RelayReservationState` — that path catches both explicit
///   denials (where libp2p closes the circuit listener) and silent
///   failures (timeouts, transport errors).
pub fn handle_relay_client_event(
    event: relay::client::Event,
    reservation_state: &mut RelayReservationState,
    event_tx: &mpsc::Sender<SumNetEvent>,
) {
    match event {
        relay::client::Event::ReservationReqAccepted {
            relay_peer_id, ..
        } => {
            reservation_state.mark_active(relay_peer_id);
            info!(
                %relay_peer_id,
                state = ?reservation_state,
                "relay reservation accepted — we are now reachable via circuit"
            );
            // Build the circuit address for the event.
            let relay_addr: Multiaddr = format!("/p2p/{}/p2p-circuit", relay_peer_id)
                .parse()
                .expect("static relay circuit multiaddr is always valid");

            let _ = event_tx.try_send(SumNetEvent::RelayReservation {
                relay_peer_id,
                relay_addr,
            });
        }
        other => {
            debug!(?other, "relay client event");
        }
    }
}

/// Reset the reservation state when a listener closes.
///
/// Called from `swarm.rs`'s `SwarmEvent::ListenerClosed` handler. If any of
/// the closed listener's addresses contain `/p2p-circuit`, we know our
/// reservation went away — whether the relay denied us, the circuit was
/// closed, or the transport timed out. In any case, reset so AutoNAT's
/// next Private tick can retry.
pub fn handle_listener_closed_for_reservation(
    closed_addrs: &[Multiaddr],
    reservation_state: &mut RelayReservationState,
) -> bool {
    let is_circuit = closed_addrs
        .iter()
        .any(|a| a.iter().any(|p| matches!(p, Protocol::P2pCircuit)));

    if is_circuit && reservation_state.is_busy() {
        let prior_peer = reservation_state.peer();
        reservation_state.reset();
        info!(
            ?prior_peer,
            "circuit listener closed — resetting reservation state for retry"
        );
        true
    } else {
        false
    }
}

// ── DCUtR event handler ──────────────────────────────────────────────────────

/// Process DCUtR (Direct Connection Upgrade through Relay) events.
///
/// A successful upgrade means the relay circuit has been replaced with a
/// direct QUIC connection via UDP hole-punching.
pub fn handle_dcutr_event(
    event: dcutr::Event,
    event_tx: &mpsc::Sender<SumNetEvent>,
) {
    match event.result {
        Ok(_connection_id) => {
            info!(
                remote_peer_id = %event.remote_peer_id,
                "DCUtR hole-punch succeeded — direct connection established"
            );
            let _ = event_tx.try_send(SumNetEvent::HolePunchSucceeded {
                peer_id: event.remote_peer_id,
            });
        }
        Err(error) => {
            let err_str = error.to_string();
            warn!(
                remote_peer_id = %event.remote_peer_id,
                error = %err_str,
                "DCUtR hole-punch failed — staying on relay circuit"
            );
            let _ = event_tx.try_send(SumNetEvent::HolePunchFailed {
                peer_id: event.remote_peer_id,
                error: err_str,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_status_unknown_is_default() {
        let a = NatStatus::Unknown;
        let b = NatStatus::Unknown;
        assert_eq!(a, b);
        assert_ne!(a, NatStatus::Private);
    }

    #[test]
    fn build_autonat_smoke() {
        let _ = build_autonat(PeerId::random());
    }

    #[test]
    fn build_relay_server_disabled_is_inert() {
        let _ = build_relay_server(PeerId::random(), false);
    }

    #[test]
    fn build_relay_server_enabled_constructs() {
        let _ = build_relay_server(PeerId::random(), true);
    }

    #[test]
    fn relay_circuit_multiaddr_format() {
        let peer = PeerId::random();
        let addr: Multiaddr = format!("/p2p/{}/p2p-circuit", peer)
            .parse()
            .expect("must always parse");
        assert!(addr.to_string().contains("p2p-circuit"));
    }

    // ── build_circuit_listen_addr tests ───────────────────────────────

    #[test]
    fn circuit_from_bare_tcp_base() {
        let peer = PeerId::random();
        let base: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);

        // Final component must be /p2p-circuit.
        let last = circuit.iter().last().unwrap();
        assert!(matches!(last, Protocol::P2pCircuit));

        // Must include /p2p/<peer> before the circuit component.
        let has_matching_p2p = circuit.iter().any(|p| matches!(p, Protocol::P2p(pid) if pid == peer));
        assert!(has_matching_p2p, "expected /p2p/<relay_peer> segment, got: {circuit}");

        // Full string form check.
        let expected = format!("/ip4/1.2.3.4/tcp/4001/p2p/{peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);
    }

    #[test]
    fn circuit_from_tcp_with_p2p_base() {
        // Input already has /p2p/<peer>; helper must NOT append it again.
        let peer = PeerId::random();
        let base: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{peer}")
            .parse()
            .unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);

        let expected = format!("/ip4/1.2.3.4/tcp/4001/p2p/{peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);

        // There should be exactly one P2p component.
        let p2p_count = circuit.iter().filter(|p| matches!(p, Protocol::P2p(_))).count();
        assert_eq!(p2p_count, 1);
    }

    #[test]
    fn circuit_from_quic_base() {
        let peer = PeerId::random();
        let base: Multiaddr = "/ip4/1.2.3.4/udp/4001/quic-v1".parse().unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);

        let expected = format!("/ip4/1.2.3.4/udp/4001/quic-v1/p2p/{peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);
    }

    #[test]
    fn already_circuit_is_idempotent() {
        let peer = PeerId::random();
        let base: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{peer}/p2p-circuit")
            .parse()
            .unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);
        // Must return unchanged — no duplicated /p2p-circuit suffix.
        assert_eq!(circuit, base);

        let circuit_count = circuit.iter().filter(|p| matches!(p, Protocol::P2pCircuit)).count();
        assert_eq!(circuit_count, 1);
    }

    #[test]
    fn circuit_from_dns_base() {
        // DNS-based relay base — important because real-world relays often
        // advertise DNS addresses (e.g. bootstrap nodes under a stable name).
        let peer = PeerId::random();
        let base: Multiaddr = "/dns4/relay.example.com/tcp/4001".parse().unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);

        let expected = format!("/dns4/relay.example.com/tcp/4001/p2p/{peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);

        // Final component must still be /p2p-circuit regardless of base shape.
        assert!(matches!(circuit.iter().last().unwrap(), Protocol::P2pCircuit));
    }

    #[test]
    fn circuit_from_dns6_base() {
        let peer = PeerId::random();
        let base: Multiaddr = "/dns6/relay.example.com/tcp/4001".parse().unwrap();
        let circuit = build_circuit_listen_addr(&base, peer);

        let expected = format!("/dns6/relay.example.com/tcp/4001/p2p/{peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);
    }

    #[test]
    fn circuit_strips_mismatched_p2p_id() {
        // If the base carries a DIFFERENT peer id than our intended relay,
        // we MUST strip the mismatched id and use `relay_peer`. Blindly
        // keeping the base's id would reserve on the wrong peer.
        let wrong_peer = PeerId::random();
        let right_peer = PeerId::random();
        assert_ne!(wrong_peer, right_peer);
        let base: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{wrong_peer}")
            .parse()
            .unwrap();
        let circuit = build_circuit_listen_addr(&base, right_peer);

        let expected = format!("/ip4/1.2.3.4/tcp/4001/p2p/{right_peer}/p2p-circuit");
        assert_eq!(circuit.to_string(), expected);

        // No trace of wrong_peer in the final multiaddr.
        let has_wrong = circuit
            .iter()
            .any(|p| matches!(p, Protocol::P2p(pid) if pid == wrong_peer));
        assert!(!has_wrong, "mismatched peer id must be stripped");

        // Exactly one /p2p/ component.
        let p2p_count = circuit
            .iter()
            .filter(|p| matches!(p, Protocol::P2p(_)))
            .count();
        assert_eq!(p2p_count, 1);
    }

    // ── RelayReservationState state machine tests ───────────────────────

    #[test]
    fn reservation_state_default_is_none() {
        assert_eq!(RelayReservationState::default(), RelayReservationState::None);
        assert!(!RelayReservationState::default().is_busy());
        assert_eq!(RelayReservationState::default().peer(), None);
    }

    #[test]
    fn reservation_state_pending_to_active_happy_path() {
        let p = PeerId::random();
        let mut s = RelayReservationState::None;

        // Request → Pending.
        s.mark_pending(p);
        assert_eq!(s, RelayReservationState::Pending(p));
        assert!(s.is_busy());
        assert_eq!(s.peer(), Some(p));

        // Relay accepts → Active.
        s.mark_active(p);
        assert_eq!(s, RelayReservationState::Active(p));
        assert!(s.is_busy());
        assert_eq!(s.peer(), Some(p));
    }

    #[test]
    fn reservation_state_mark_active_ignores_peer_mismatch() {
        // A stray ReservationReqAccepted for some OTHER peer must not
        // corrupt the state we're tracking for `p1`.
        let p1 = PeerId::random();
        let p2 = PeerId::random();
        assert_ne!(p1, p2);

        let mut s = RelayReservationState::None;
        s.mark_pending(p1);

        s.mark_active(p2);
        assert_eq!(s, RelayReservationState::Pending(p1), "mismatch must not upgrade");

        s.mark_active(p1);
        assert_eq!(s, RelayReservationState::Active(p1), "matching peer upgrades");
    }

    #[test]
    fn reservation_state_mark_active_from_none_is_noop() {
        // Accept events without a preceding Pending must NOT transition.
        // (Guards against out-of-order events post-reset.)
        let p = PeerId::random();
        let mut s = RelayReservationState::None;

        s.mark_active(p);
        assert_eq!(s, RelayReservationState::None);
        assert!(!s.is_busy());
    }

    #[test]
    fn reservation_state_reset_from_pending_allows_retry() {
        // Regression test for Codex's wedging concern:
        // Pending → reset (denial) → must allow a NEW mark_pending.
        let p1 = PeerId::random();
        let p2 = PeerId::random();
        assert_ne!(p1, p2);

        let mut s = RelayReservationState::None;
        s.mark_pending(p1);
        assert!(s.is_busy());

        // Simulate denial / listener close → reset.
        s.reset();
        assert_eq!(s, RelayReservationState::None);
        assert!(!s.is_busy());

        // Retry with a different peer.
        s.mark_pending(p2);
        assert_eq!(s, RelayReservationState::Pending(p2));
        assert!(s.is_busy());
    }

    #[test]
    fn reservation_state_reset_from_active_allows_retry() {
        // Active → reset (relay closed our circuit) → must allow retry.
        let p = PeerId::random();
        let mut s = RelayReservationState::None;
        s.mark_pending(p);
        s.mark_active(p);
        assert_eq!(s, RelayReservationState::Active(p));

        s.reset();
        assert_eq!(s, RelayReservationState::None);
        assert!(!s.is_busy());

        // Free to re-request.
        s.mark_pending(p);
        assert_eq!(s, RelayReservationState::Pending(p));
    }

    #[test]
    fn handle_listener_closed_resets_when_circuit_address_present() {
        // The full state-reset flow: pretend a circuit listener closed,
        // and verify the reservation state is swept back to None so the
        // node can retry.
        let peer = PeerId::random();
        let mut state = RelayReservationState::None;
        state.mark_pending(peer);
        assert!(state.is_busy());

        let circuit_addr: Multiaddr =
            format!("/ip4/1.2.3.4/tcp/4001/p2p/{peer}/p2p-circuit")
                .parse()
                .unwrap();
        let changed = handle_listener_closed_for_reservation(
            std::slice::from_ref(&circuit_addr),
            &mut state,
        );
        assert!(changed, "helper must report it reset the state");
        assert_eq!(state, RelayReservationState::None);
    }

    #[test]
    fn handle_listener_closed_ignores_non_circuit_listeners() {
        // A regular TCP/QUIC listener closing must NOT clobber an active
        // circuit reservation on a different listener.
        let peer = PeerId::random();
        let mut state = RelayReservationState::None;
        state.mark_pending(peer);
        state.mark_active(peer);

        let tcp_addr: Multiaddr = "/ip4/0.0.0.0/tcp/4001".parse().unwrap();
        let changed = handle_listener_closed_for_reservation(
            std::slice::from_ref(&tcp_addr),
            &mut state,
        );
        assert!(!changed, "non-circuit listener close must not reset");
        assert_eq!(state, RelayReservationState::Active(peer));
    }

    #[test]
    fn handle_listener_closed_noop_when_state_already_none() {
        // Benign edge case: listener closes but we already reset — stay None.
        let mut state = RelayReservationState::None;
        let circuit_addr: Multiaddr = format!("/ip4/1.2.3.4/tcp/4001/p2p/{}/p2p-circuit", PeerId::random())
            .parse()
            .unwrap();
        let changed = handle_listener_closed_for_reservation(
            std::slice::from_ref(&circuit_addr),
            &mut state,
        );
        assert!(!changed);
        assert_eq!(state, RelayReservationState::None);
    }

    // ── RelayCandidate confirmation gate ────────────────────────────────

    #[test]
    fn relay_candidate_default_is_unconfirmed() {
        let c = RelayCandidate::default();
        assert!(c.addrs.is_empty());
        assert!(!c.confirmed);
    }

    #[test]
    fn relay_candidate_bootstrap_then_identify_sequence() {
        // Mirrors the real flow: bootstrap seeds the candidate with
        // confirmed=false and an address; identify later flips confirmed
        // to true without discarding addresses.
        let addr: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();

        // 1. Bootstrap seeds.
        let mut c = RelayCandidate::default();
        c.addrs.push(addr.clone());
        assert!(!c.confirmed, "bootstrap must not claim confirmed");

        // 2. Identify confirms.
        c.confirmed = true;
        assert!(c.confirmed);
        assert_eq!(c.addrs, vec![addr]);
    }
}
