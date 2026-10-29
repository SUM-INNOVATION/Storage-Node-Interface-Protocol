//! Enforcement of the network policy (SNIP #79). See [`crate::policy`] for the
//! rules and for why each piece below sits where it does.
//!
//! * [`PolicyTransport`] — the outermost transport layer. Every
//!   `Transport::dial` passes through it; this is the decisive check.
//! * [`PolicyBehaviour`] — the first field of
//!   [`crate::behaviour::LocalMeshBehaviour`]. Judges inbound connections,
//!   re-checks completed outbound ones, and tells the transport gate which
//!   source started the dial in progress.
//! * [`Gated`] — wraps a behaviour that supplies peer addresses (mDNS,
//!   Identify, Kademlia, DCUtR, AutoNAT, relay client). Filters the addresses
//!   it contributes to a dial and the peer addresses it publishes to other
//!   behaviours, and labels the dials it starts.
//!
//! With no allow-list every one of these is a pass-through.

use std::convert::Infallible;
use std::io;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::transport::{
    Boxed, DialOpts, ListenerId, PortUse, TransportError, TransportEvent,
};
use libp2p::core::{Endpoint, Transport};
use libp2p::swarm::{
    ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent,
    THandlerOutEvent, ToSwarm, dummy,
};
use libp2p::{Multiaddr, PeerId};
use tracing::debug;

use crate::policy::{NetPolicy, OUTBOUND_ALLOW_FLAG, PolicySource};

/// The reason carried by a connection the policy refused.
#[derive(Debug)]
pub struct PolicyDenied {
    addr: Multiaddr,
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refused by the {OUTBOUND_ALLOW_FLAG} policy: {}",
            self.addr
        )
    }
}

impl std::error::Error for PolicyDenied {}

fn denied(addr: &Multiaddr) -> ConnectionDenied {
    ConnectionDenied::new(PolicyDenied { addr: addr.clone() })
}

// ── Transport gate ───────────────────────────────────────────────────────────

/// The transport every swarm connection goes through, gated by the policy.
///
/// It wraps the fully composed stack (relay client, DNS, TCP, QUIC, with the
/// connection timeout), so the address it judges is the one the swarm is about
/// to dial — after every behaviour has added its addresses, and before DNS
/// resolution or any packet. A refused dial fails with `PermissionDenied`
/// before a socket is touched; the swarm reports it per address in
/// `OutgoingConnectionError` and moves on to the next address of the dial.
pub struct PolicyTransport {
    inner: Boxed<(PeerId, StreamMuxerBox)>,
    policy: Arc<NetPolicy>,
}

impl PolicyTransport {
    pub fn new(inner: Boxed<(PeerId, StreamMuxerBox)>, policy: Arc<NetPolicy>) -> Self {
        Self { inner, policy }
    }
}

impl Transport for PolicyTransport {
    type Output = (PeerId, StreamMuxerBox);
    type Error = io::Error;
    type ListenerUpgrade = <Boxed<(PeerId, StreamMuxerBox)> as Transport>::ListenerUpgrade;
    type Dial = <Boxed<(PeerId, StreamMuxerBox)> as Transport>::Dial;

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        // Listeners are unchanged by the policy.
        self.inner.listen_on(id, addr)
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        self.inner.remove_listener(id)
    }

    fn dial(
        &mut self,
        addr: Multiaddr,
        opts: DialOpts,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        if let Err(source) = self.policy.check_transport_dial(&addr, opts.role) {
            return Err(TransportError::Other(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refused by the {OUTBOUND_ALLOW_FLAG} policy (source: {})",
                    source.label()
                ),
            )));
        }
        self.inner.dial(addr, opts)
    }

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

// ── Connection-level behaviour ───────────────────────────────────────────────

/// Connection-level policy checks. Carries no protocol and no state of its
/// own beyond the shared [`NetPolicy`]; its handler is `dummy`.
///
/// It must be the **first** field of the composed behaviour: the derived
/// behaviour asks its fields in declaration order, so a connection refused
/// here is refused before any other behaviour builds a handler or records
/// state for it, and the dial label is current before any address is dialed.
pub struct PolicyBehaviour {
    policy: Arc<NetPolicy>,
}

impl PolicyBehaviour {
    pub fn new(policy: Arc<NetPolicy>) -> Self {
        Self { policy }
    }
}

impl NetworkBehaviour for PolicyBehaviour {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_pending_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        // Before the handshake: refuse what no entry could allow, whoever the
        // peer turns out to be.
        if self.policy.inbound_allowed(local_addr, remote_addr, None) {
            Ok(())
        } else {
            Err(denied(remote_addr))
        }
    }

    fn handle_established_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        peer: PeerId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        // After the handshake the peer is known, so peer-pinned entries are
        // enforced.
        if self
            .policy
            .inbound_allowed(local_addr, remote_addr, Some(peer))
        {
            Ok(dummy::ConnectionHandler)
        } else {
            Err(denied(remote_addr))
        }
    }

    fn handle_pending_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        _maybe_peer: Option<PeerId>,
        _addresses: &[Multiaddr],
        _effective_role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.policy.begin_dial(connection_id);
        Ok(vec![])
    }

    fn handle_established_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        _role_override: Endpoint,
        _port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        if self.policy.outbound_established_allowed(addr, peer) {
            Ok(dummy::ConnectionHandler)
        } else {
            Err(denied(addr))
        }
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        if let FromSwarm::DialFailure(f) = event {
            self.policy.forget_dial(f.connection_id);
        }
    }

    fn on_connection_handler_event(
        &mut self,
        _peer_id: PeerId,
        _connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {}
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        Poll::Pending
    }
}

// ── Address-source wrapper ───────────────────────────────────────────────────

/// A behaviour whose address output is filtered by the policy and attributed
/// to `source`.
///
/// Dereferences to the inner behaviour, so call sites such as
/// `behaviour_mut().kademlia.add_address(..)` are unchanged, and its
/// `ToSwarm` is the inner one, so the derived event enum keeps its variants.
pub struct Gated<B> {
    inner: B,
    source: PolicySource,
    policy: Arc<NetPolicy>,
}

impl<B> Gated<B> {
    pub fn new(inner: B, source: PolicySource, policy: Arc<NetPolicy>) -> Self {
        Self {
            inner,
            source,
            policy,
        }
    }

    /// The source label this wrapper reports refusals under.
    pub fn source(&self) -> PolicySource {
        self.source
    }
}

impl<B> Deref for Gated<B> {
    type Target = B;
    fn deref(&self) -> &B {
        &self.inner
    }
}

impl<B> DerefMut for Gated<B> {
    fn deref_mut(&mut self) -> &mut B {
        &mut self.inner
    }
}

impl<B: NetworkBehaviour> NetworkBehaviour for Gated<B> {
    type ConnectionHandler = B::ConnectionHandler;
    type ToSwarm = B::ToSwarm;

    fn handle_pending_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.inner
            .handle_pending_inbound_connection(connection_id, local_addr, remote_addr)
    }

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.inner.handle_established_inbound_connection(
            connection_id,
            peer,
            local_addr,
            remote_addr,
        )
    }

    fn handle_pending_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        maybe_peer: Option<PeerId>,
        addresses: &[Multiaddr],
        effective_role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        let mut offered = self.inner.handle_pending_outbound_connection(
            connection_id,
            maybe_peer,
            addresses,
            effective_role,
        )?;
        if self.policy.is_restricted() {
            offered.retain(|a| self.policy.admit_dial(self.source, a, maybe_peer));
        }
        Ok(offered)
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role_override: Endpoint,
        port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.inner.handle_established_outbound_connection(
            connection_id,
            peer,
            addr,
            role_override,
            port_use,
        )
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        self.inner.on_swarm_event(event)
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.inner
            .on_connection_handler_event(peer_id, connection_id, event)
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        loop {
            match self.inner.poll(cx) {
                Poll::Ready(ToSwarm::Dial { opts }) => {
                    self.policy.label_dial(opts.connection_id(), self.source);
                    return Poll::Ready(ToSwarm::Dial { opts });
                }
                // A peer address this behaviour learned is published to every
                // other behaviour (request-response keeps a cache of them and
                // dials from it). A refused one is withheld, so it cannot come
                // back through a behaviour that is not wrapped. Not counted:
                // the address was already judged where it was learned.
                Poll::Ready(ToSwarm::NewExternalAddrOfPeer { peer_id, address })
                    if !self.policy.allows_quietly(&address, Some(peer_id)) =>
                {
                    debug!(
                        source = self.source.label(),
                        %peer_id,
                        %address,
                        "network policy: peer address withheld from other behaviours"
                    );
                    continue;
                }
                other => return other,
            }
        }
    }
}
