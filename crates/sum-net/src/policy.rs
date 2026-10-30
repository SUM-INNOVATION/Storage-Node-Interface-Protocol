//! Network policy for controlled networks (SNIP #79): configurable mDNS and an
//! outbound address allow-list.
//!
//! # What this is, and what it is not
//!
//! These are **application-level routing controls for cooperative, controlled
//! networks**. They make an honest `sum-node` keep to the endpoints its
//! operator configured, and they make every refused route observable. They
//! are **not an operating-system security boundary**: a modified or malicious
//! client can ignore them, and nothing here constrains any other process on
//! the host. Use a firewall or a sandbox when containment is the requirement.
//!
//! # Where the allow-list is enforced
//!
//! With no allow-list configured every check below is skipped and the node
//! behaves exactly as before. With one configured:
//!
//! 1. **The transport** ([`crate::gate::PolicyTransport`]) wraps the whole
//!    transport stack — relay client, DNS, TCP and QUIC — and judges every
//!    address handed to `Transport::dial`, after the swarm has merged the
//!    addresses every behaviour contributed. Nothing reaches a socket without
//!    passing it, whichever behaviour asked: bootstrap, mDNS, Identify,
//!    Kademlia, request-response, gossipsub, AutoNAT dial-back, DCUtR
//!    hole-punching (both roles, QUIC included) and relay-client dials to a
//!    relay. This is the decisive control.
//! 2. **Address sources** are filtered before their addresses are stored or
//!    offered for a dial, so a refused address does not linger in Kademlia,
//!    relay candidates or the request-response address caches: bootstrap
//!    entries, mDNS discoveries, Identify listen addresses and Kademlia
//!    routing updates are checked where `SumSwarm` handles them, and the
//!    address-supplying behaviours are wrapped ([`crate::gate::Gated`]) so
//!    the addresses they contribute to a dial, and the peer addresses they
//!    publish to other behaviours, are filtered too.
//! 3. **Connections in the other direction** are judged by
//!    [`crate::gate::PolicyBehaviour`]: a direct inbound connection is kept
//!    only when the remote address is allowed, and a relayed one only when its
//!    relay is. Without this an allowed peer could open the second route
//!    itself, by dialing in from an address that is not allowed (DCUtR, its
//!    own direct-dial shortcut, or Kademlia all do).
//!
//! # Matching rules
//!
//! An entry is either an IP network or a multiaddr prefix.
//!
//! * **IP network** — `127.0.0.1`, `10.0.0.0/8`, `::1`, `2001:db8::/32`. A
//!   bare address is a single host (`/32` or `/128`). It matches an address
//!   whose host is `/ip4` or `/ip6` inside the network, on any transport and
//!   port, for any peer. An IPv4-mapped IPv6 host (`::ffff:a.b.c.d`) also
//!   matches IPv4 networks.
//! * **Multiaddr prefix** — a host (`/ip4`, `/ip6`, `/dns`, `/dns4`, `/dns6`,
//!   `/dnsaddr`), then optionally `/tcp/<port>`, `/udp/<port>` or
//!   `/udp/<port>/quic-v1`, then optionally `/p2p/<peer-id>`. It matches an
//!   address that begins with the same components (DNS names compare
//!   case-insensitively; a host-only entry therefore allows every port on that
//!   host, and `/udp/<port>` allows QUIC on that port). An entry ending in
//!   `/p2p/<peer-id>` additionally requires the dial to name that peer; libp2p
//!   then refuses the connection if the remote proves to be someone else.
//!
//! DNS names are judged **before** resolution, because that is the form the
//! transport is asked to dial: a `/dns4/...` address matches only a DNS entry
//! with the same name, never an IP network, and an allowed name is trusted to
//! resolve wherever its records point.
//!
//! A relayed address (`<relay address>/p2p-circuit/p2p/<peer>`) is judged by
//! its relay part alone, because that is where the connection goes; the relay
//! connection is itself a dial that is judged again. A relay part without a
//! host matches nothing. Entries cannot contain `/p2p-circuit`, and an entry
//! that names only a peer (`/p2p/<peer-id>`) is refused: an allowed peer can
//! advertise any address, which is the whole problem this policy exists for.
//!
//! An address that does not start with a host (`/unix`, `/memory`,
//! `/ip6zone`, ...) is never allowed while a policy is set.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use libp2p::core::Endpoint;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::ConnectionId;
use libp2p::{Multiaddr, PeerId};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use sum_types::config::NetConfig;

/// The CLI flag that carries the allow-list. Named in errors and logs so an
/// operator can find the knob that produced them.
pub const OUTBOUND_ALLOW_FLAG: &str = "--outbound-allow";

/// Environment variable equivalent of [`OUTBOUND_ALLOW_FLAG`].
pub const OUTBOUND_ALLOW_ENV: &str = "SUM_OUTBOUND_ALLOW";

/// How many distinct `(source, address)` refusals are logged at `warn` before
/// the rest are logged at `debug` only. Counters are never capped.
const WARN_DEDUP_CAP: usize = 1024;

/// Upper bound on dial labels awaiting their dial. Labels are consumed when
/// the dial starts or fails, so this only bounds a leak that should not occur.
const DIAL_LABEL_CAP: usize = 4096;

/// Audit channel depth. A receiver that falls further behind loses the oldest
/// events (and is told so by `broadcast`); counters are unaffected.
const AUDIT_CHANNEL_CAPACITY: usize = 1024;

// ── Parse errors ─────────────────────────────────────────────────────────────

/// A malformed allow-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyParseError {
    entry: String,
    reason: String,
}

impl PolicyParseError {
    fn new(entry: &str, reason: impl Into<String>) -> Self {
        Self {
            entry: entry.to_string(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for PolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {OUTBOUND_ALLOW_FLAG} entry `{}`: {}",
            self.entry, self.reason
        )
    }
}

impl std::error::Error for PolicyParseError {}

// ── Entries ──────────────────────────────────────────────────────────────────

/// One allow-list entry. See the module documentation for the matching rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowEntry {
    /// An IP network. `addr` is stored with the host bits cleared.
    Network { addr: IpAddr, prefix: u8 },
    /// A multiaddr prefix: a host, optionally a port (and `quic-v1`), and an
    /// optional peer pin taken from a trailing `/p2p/<peer-id>`.
    Prefix {
        components: Vec<Protocol<'static>>,
        peer: Option<PeerId>,
    },
}

impl FromStr for AllowEntry {
    type Err = PolicyParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(PolicyParseError::new(raw, "empty entry"));
        }
        let parsed = if s.starts_with('/') {
            parse_prefix(s)
        } else {
            parse_network(s)
        };
        parsed.map_err(|reason| PolicyParseError::new(s, reason))
    }
}

impl fmt::Display for AllowEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllowEntry::Network { addr, prefix } => write!(f, "{addr}/{prefix}"),
            AllowEntry::Prefix { components, peer } => {
                let mut ma: Multiaddr = components.iter().cloned().collect();
                if let Some(p) = peer {
                    ma.push(Protocol::P2p(*p));
                }
                write!(f, "{ma}")
            }
        }
    }
}

fn parse_network(s: &str) -> Result<AllowEntry, String> {
    let (ip_part, prefix_part) = match s.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix)),
        None => (s, None),
    };
    let addr: IpAddr = ip_part.parse().map_err(|_| {
        format!(
            "`{ip_part}` is not an IP address or CIDR (multiaddr entries start with `/`, \
             e.g. /ip4/127.0.0.1/tcp/4001)"
        )
    })?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix_part {
        None => max,
        Some(p) => {
            let n: u8 = p
                .parse()
                .map_err(|_| format!("`{p}` is not a prefix length"))?;
            if n > max {
                return Err(format!("prefix length {n} exceeds {max}"));
            }
            n
        }
    };
    Ok(AllowEntry::Network {
        addr: mask(addr, prefix),
        prefix,
    })
}

fn parse_prefix(s: &str) -> Result<AllowEntry, String> {
    let ma: Multiaddr = s
        .parse()
        .map_err(|e| format!("not a valid multiaddr ({e})"))?;
    let all: Vec<Protocol<'static>> = ma.iter().map(|p| p.acquire()).collect();

    if all.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
        return Err(
            "relayed (/p2p-circuit) addresses cannot be listed; list the relay's own address \
             instead — a relayed address is allowed when its relay part is"
                .into(),
        );
    }

    let (body, peer) = match all.split_last() {
        Some((Protocol::P2p(p), rest)) => (rest.to_vec(), Some(*p)),
        _ => (all.clone(), None),
    };

    let Some(host) = body.first() else {
        return Err(
            "a peer ID alone does not restrict addresses (an allowed peer can advertise any \
             address); name the host too, e.g. /ip4/127.0.0.1/tcp/4001/p2p/<peer-id>"
                .into(),
        );
    };
    if !is_host(host) {
        return Err(format!(
            "must start with /ip4, /ip6, /dns, /dns4, /dns6 or /dnsaddr, not /{}",
            host.tag()
        ));
    }

    match &body[1..] {
        [] => {}
        [Protocol::Tcp(port)] | [Protocol::Udp(port)] | [Protocol::Udp(port), Protocol::QuicV1] => {
            if *port == 0 {
                return Err(
                    "port 0 is not a wildcard; leave the port out to allow every port on the host"
                        .into(),
                );
            }
        }
        _ => {
            return Err(
                "unsupported form after the host: an entry may continue with /tcp/<port>, \
                 /udp/<port> or /udp/<port>/quic-v1, then an optional final /p2p/<peer-id>"
                    .into(),
            );
        }
    }

    Ok(AllowEntry::Prefix {
        components: body,
        peer,
    })
}

fn is_host(p: &Protocol<'_>) -> bool {
    matches!(
        p,
        Protocol::Ip4(_)
            | Protocol::Ip6(_)
            | Protocol::Dns(_)
            | Protocol::Dns4(_)
            | Protocol::Dns6(_)
            | Protocol::Dnsaddr(_)
    )
}

fn mask(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(v4) => {
            let m = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & m))
        }
        IpAddr::V6(v6) => {
            let m = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & m))
        }
    }
}

fn network_contains(net: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => mask(ip, prefix) == net,
        (IpAddr::V4(_), IpAddr::V6(v6)) => v6
            .to_ipv4_mapped()
            .is_some_and(|v4| mask(IpAddr::V4(v4), prefix) == net),
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

fn dns_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

fn same_component(entry: &Protocol<'_>, addr: &Protocol<'_>) -> bool {
    match (entry, addr) {
        (Protocol::Dns(a), Protocol::Dns(b))
        | (Protocol::Dns4(a), Protocol::Dns4(b))
        | (Protocol::Dns6(a), Protocol::Dns6(b))
        | (Protocol::Dnsaddr(a), Protocol::Dnsaddr(b)) => dns_eq(a, b),
        _ => entry == addr,
    }
}

/// How a peer pin on an entry is treated when the remote peer is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerPin {
    /// The address (or the dial) must name the pinned peer.
    Enforce,
    /// The peer is not known yet (an inbound connection before the handshake);
    /// a pinned entry may still match once it is. Used only to refuse early
    /// what no entry could ever allow.
    Defer,
}

impl AllowEntry {
    fn matches(&self, transport: &[Protocol<'_>], peer: Option<PeerId>, pin: PeerPin) -> bool {
        match self {
            AllowEntry::Network { addr, prefix } => match transport.first() {
                Some(Protocol::Ip4(ip)) => network_contains(*addr, *prefix, IpAddr::V4(*ip)),
                Some(Protocol::Ip6(ip)) => network_contains(*addr, *prefix, IpAddr::V6(*ip)),
                _ => false,
            },
            AllowEntry::Prefix {
                components,
                peer: wanted,
            } => {
                let prefix_ok = components.len() <= transport.len()
                    && components
                        .iter()
                        .zip(transport)
                        .all(|(e, a)| same_component(e, a));
                let peer_ok = match (wanted, peer) {
                    (None, _) => true,
                    (Some(w), Some(p)) => *w == p,
                    (Some(_), None) => pin == PeerPin::Defer,
                };
                prefix_ok && peer_ok
            }
        }
    }
}

// ── The allow-list ───────────────────────────────────────────────────────────

/// A parsed, non-empty allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddrPolicy {
    entries: Vec<AllowEntry>,
}

impl AddrPolicy {
    /// Parse a list of entries. Blank items (from a trailing comma, say) are
    /// skipped; a list with no entries left is "no policy" (`None`), which is
    /// the unrestricted default.
    pub fn parse<I, S>(items: I) -> Result<Option<Self>, PolicyParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut entries = Vec::new();
        for item in items {
            let item = item.as_ref();
            if item.trim().is_empty() {
                continue;
            }
            entries.push(item.parse::<AllowEntry>()?);
        }
        Ok((!entries.is_empty()).then_some(Self { entries }))
    }

    /// The parsed entries, in the order given.
    pub fn entries(&self) -> &[AllowEntry] {
        &self.entries
    }

    /// Is a connection to `addr` allowed?
    ///
    /// `peer_hint` is the peer a direct dial targets when the address itself
    /// does not end in `/p2p/<peer-id>` (the swarm appends it before the
    /// transport sees the address, so this matters only to callers judging an
    /// address on its own). It is ignored for relayed addresses, where the
    /// destination is not the peer the connection goes to.
    pub fn allows(&self, addr: &Multiaddr, peer_hint: Option<PeerId>) -> bool {
        self.decide(addr, peer_hint, PeerPin::Enforce)
    }

    fn decide(&self, addr: &Multiaddr, peer_hint: Option<PeerId>, pin: PeerPin) -> bool {
        let comps: Vec<Protocol<'_>> = addr.iter().collect();
        let (segment, relayed) = match comps.iter().position(|p| matches!(p, Protocol::P2pCircuit))
        {
            Some(i) => (&comps[..i], true),
            None => (&comps[..], false),
        };
        let (transport, peer) = match segment.split_last() {
            Some((Protocol::P2p(p), rest)) => (rest, Some(*p)),
            _ => (segment, if relayed { None } else { peer_hint }),
        };
        let Some(host) = transport.first() else {
            return false;
        };
        if !is_host(host) || transport.iter().any(|p| matches!(p, Protocol::P2p(_))) {
            return false;
        }
        self.entries.iter().any(|e| e.matches(transport, peer, pin))
    }
}

// ── Sources, stages, counters ────────────────────────────────────────────────

/// Where a refused address or connection came from. Fixed set: nothing a
/// remote peer sends can become a new label, so the counter set is bounded by
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicySource {
    /// `--bootstrap-peer` / `SUM_BOOTSTRAP_PEERS`.
    Bootstrap,
    /// mDNS discovery.
    Mdns,
    /// Addresses a peer advertised through Identify.
    Identify,
    /// Kademlia routing table or query results.
    Kad,
    /// Automatic direct dials: the Identify direct-dial shortcut, DCUtR
    /// hole-punching, AutoNAT dial-back and relay-client dials to a relay.
    Autodial,
    /// A connection a remote peer opened to this node.
    Inbound,
    /// A dial whose initiator carries no label (request-response, gossipsub).
    Other,
}

impl PolicySource {
    /// Every source, in counter order.
    pub const ALL: [PolicySource; 7] = [
        PolicySource::Bootstrap,
        PolicySource::Mdns,
        PolicySource::Identify,
        PolicySource::Kad,
        PolicySource::Autodial,
        PolicySource::Inbound,
        PolicySource::Other,
    ];

    /// Stable lower-case label used in logs.
    pub fn label(self) -> &'static str {
        match self {
            PolicySource::Bootstrap => "bootstrap",
            PolicySource::Mdns => "mdns",
            PolicySource::Identify => "identify",
            PolicySource::Kad => "kad",
            PolicySource::Autodial => "autodial",
            PolicySource::Inbound => "inbound",
            PolicySource::Other => "other",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// At which point a refusal happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalStage {
    /// An address was learned and refused before it was stored or offered
    /// for any dial.
    Admission,
    /// An address was refused as a dial target.
    Dial,
    /// A connection opened by a remote peer was refused.
    Inbound,
}

impl RefusalStage {
    /// Stable lower-case label used in logs.
    pub fn label(self) -> &'static str {
        match self {
            RefusalStage::Admission => "admission",
            RefusalStage::Dial => "dial",
            RefusalStage::Inbound => "inbound",
        }
    }
}

/// Snapshot of refusal counts by source, as fixed fields.
///
/// Counts refusals, not distinct addresses: an address offered again (for
/// example on every dial attempt to a peer) is counted again.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PolicyRefusals {
    pub bootstrap: u64,
    pub mdns: u64,
    pub identify: u64,
    pub kad: u64,
    pub autodial: u64,
    pub inbound: u64,
    pub other: u64,
}

impl PolicyRefusals {
    /// Sum over every source.
    pub fn total(&self) -> u64 {
        self.bootstrap
            + self.mdns
            + self.identify
            + self.kad
            + self.autodial
            + self.inbound
            + self.other
    }

    /// The count for one source.
    pub fn get(&self, source: PolicySource) -> u64 {
        match source {
            PolicySource::Bootstrap => self.bootstrap,
            PolicySource::Mdns => self.mdns,
            PolicySource::Identify => self.identify,
            PolicySource::Kad => self.kad,
            PolicySource::Autodial => self.autodial,
            PolicySource::Inbound => self.inbound,
            PolicySource::Other => self.other,
        }
    }
}

// ── Connection audit stream ──────────────────────────────────────────────────

/// Connection events as the swarm saw them, plus every policy refusal.
///
/// Available from [`crate::SumNet::connection_audit`]. For an outbound
/// connection `remote_addr` is the address that was dialed; for an inbound one
/// it is the remote's send-back address (for a relayed inbound connection,
/// `/p2p/<source-peer>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionAudit {
    Established {
        peer_id: PeerId,
        remote_addr: Multiaddr,
        outbound: bool,
        relayed: bool,
    },
    Closed {
        peer_id: PeerId,
        remote_addr: Multiaddr,
        outbound: bool,
        relayed: bool,
    },
    Refused {
        source: PolicySource,
        stage: RefusalStage,
        addr: Multiaddr,
    },
}

// ── Runtime state ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct DialAttribution {
    /// Dials a labelled behaviour (or `SumSwarm` itself) asked for, by the
    /// connection id the swarm will use.
    labels: HashMap<ConnectionId, PolicySource>,
    /// The label of the dial the swarm is starting right now. The swarm calls
    /// `handle_pending_outbound_connection` and then `Transport::dial` for
    /// each address synchronously within one `Swarm::dial`, so this is the
    /// label the transport gate reports.
    current: PolicySource,
}

/// The network policy one swarm runs under, shared between the transport
/// gate, the policy behaviours and `SumSwarm`.
#[derive(Debug)]
pub struct NetPolicy {
    mdns: bool,
    allow: Option<AddrPolicy>,
    refusals: [AtomicU64; PolicySource::ALL.len()],
    audit: broadcast::Sender<ConnectionAudit>,
    dials: Mutex<DialAttribution>,
    warned: Mutex<HashSet<(PolicySource, Multiaddr)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl NetPolicy {
    /// Build from a parsed allow-list.
    pub fn new(mdns: bool, allow: Option<AddrPolicy>) -> Self {
        let (audit, _) = broadcast::channel(AUDIT_CHANNEL_CAPACITY);
        Self {
            mdns,
            allow,
            refusals: Default::default(),
            audit,
            dials: Mutex::new(DialAttribution {
                labels: HashMap::new(),
                current: PolicySource::Other,
            }),
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// Parse the policy out of a [`NetConfig`]. Fails on a malformed entry.
    pub fn from_config(config: &NetConfig) -> Result<Self, PolicyParseError> {
        Ok(Self::new(
            config.mdns,
            AddrPolicy::parse(&config.outbound_allow)?,
        ))
    }

    /// Whether mDNS discovery runs.
    pub fn mdns_enabled(&self) -> bool {
        self.mdns
    }

    /// The allow-list, if one is set.
    pub fn allow_list(&self) -> Option<&AddrPolicy> {
        self.allow.as_ref()
    }

    /// Whether an allow-list is in force.
    pub fn is_restricted(&self) -> bool {
        self.allow.is_some()
    }

    /// One-line description of the effective policy. Contains configured
    /// addresses and nothing else — no keys, no credentials.
    pub fn describe(&self) -> String {
        let mdns = if self.mdns { "on" } else { "off" };
        match &self.allow {
            None => format!("mdns={mdns} outbound_allow=unrestricted"),
            Some(p) => {
                let list: Vec<String> = p.entries().iter().map(ToString::to_string).collect();
                format!(
                    "mdns={mdns} outbound_allow=[{}] (application-level routing control, \
                     not an OS security boundary)",
                    list.join(", ")
                )
            }
        }
    }

    /// Log the effective policy once, at swarm construction.
    pub(crate) fn log_effective(&self) {
        info!("network policy: {}", self.describe());
    }

    /// Snapshot of the refusal counters.
    pub fn refusals(&self) -> PolicyRefusals {
        let get = |s: PolicySource| self.refusals[s.index()].load(Ordering::Relaxed);
        PolicyRefusals {
            bootstrap: get(PolicySource::Bootstrap),
            mdns: get(PolicySource::Mdns),
            identify: get(PolicySource::Identify),
            kad: get(PolicySource::Kad),
            autodial: get(PolicySource::Autodial),
            inbound: get(PolicySource::Inbound),
            other: get(PolicySource::Other),
        }
    }

    /// A new receiver on the connection audit stream, starting now.
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionAudit> {
        self.audit.subscribe()
    }

    pub(crate) fn audit(&self, event: ConnectionAudit) {
        // No receiver is the normal case; the event is simply not kept.
        let _ = self.audit.send(event);
    }

    fn record(&self, source: PolicySource, stage: RefusalStage, addr: &Multiaddr) {
        self.refusals[source.index()].fetch_add(1, Ordering::Relaxed);
        let first = {
            let mut warned = lock(&self.warned);
            warned.len() < WARN_DEDUP_CAP && warned.insert((source, addr.clone()))
        };
        if first {
            warn!(
                source = source.label(),
                stage = stage.label(),
                %addr,
                "network policy: address not in {OUTBOUND_ALLOW_FLAG} — refused"
            );
        } else {
            debug!(
                source = source.label(),
                stage = stage.label(),
                %addr,
                "network policy: address not in {OUTBOUND_ALLOW_FLAG} — refused (repeat)"
            );
        }
        self.audit(ConnectionAudit::Refused {
            source,
            stage,
            addr: addr.clone(),
        });
    }

    fn judge(
        &self,
        source: PolicySource,
        stage: RefusalStage,
        addr: &Multiaddr,
        peer: Option<PeerId>,
    ) -> bool {
        match &self.allow {
            None => true,
            Some(p) if p.allows(addr, peer) => true,
            Some(_) => {
                self.record(source, stage, addr);
                false
            }
        }
    }

    /// Admit an address learned from `source` into a store or event. Refused
    /// addresses are counted and logged; the caller must drop them.
    pub(crate) fn admit(
        &self,
        source: PolicySource,
        addr: &Multiaddr,
        peer: Option<PeerId>,
    ) -> bool {
        self.judge(source, RefusalStage::Admission, addr, peer)
    }

    /// Admit an address `source` offers as a dial target.
    pub(crate) fn admit_dial(
        &self,
        source: PolicySource,
        addr: &Multiaddr,
        peer: Option<PeerId>,
    ) -> bool {
        self.judge(source, RefusalStage::Dial, addr, peer)
    }

    /// Like [`Self::admit`] but silent: for echoes of addresses that were
    /// already judged (and counted) where they were first learned.
    pub(crate) fn allows_quietly(&self, addr: &Multiaddr, peer: Option<PeerId>) -> bool {
        self.allow.as_ref().is_none_or(|p| p.allows(addr, peer))
    }

    /// The transport gate's check. `Err` carries the source the refusal was
    /// attributed to.
    pub(crate) fn check_transport_dial(
        &self,
        addr: &Multiaddr,
        role: Endpoint,
    ) -> Result<(), PolicySource> {
        let Some(p) = &self.allow else {
            return Ok(());
        };
        if p.allows(addr, None) {
            return Ok(());
        }
        let mut source = lock(&self.dials).current;
        if source == PolicySource::Other && role == Endpoint::Listener {
            // A dial in the listener role is a hole-punch.
            source = PolicySource::Autodial;
        }
        self.record(source, RefusalStage::Dial, addr);
        Err(source)
    }

    /// Judge a connection a remote peer opened. A relayed connection is judged
    /// by the relay part of `local_addr` (the circuit we listen on); a direct
    /// one by `remote_addr`. `peer` is `None` before the handshake, when a
    /// peer-pinned entry is given the benefit of the doubt.
    pub(crate) fn inbound_allowed(
        &self,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
        peer: Option<PeerId>,
    ) -> bool {
        let Some(p) = &self.allow else {
            return true;
        };
        let relayed = local_addr.iter().any(|x| matches!(x, Protocol::P2pCircuit));
        let (judged, ok) = if relayed {
            (local_addr, p.decide(local_addr, None, PeerPin::Enforce))
        } else {
            let pin = if peer.is_some() {
                PeerPin::Enforce
            } else {
                PeerPin::Defer
            };
            (remote_addr, p.decide(remote_addr, peer, pin))
        };
        if !ok {
            self.record(PolicySource::Inbound, RefusalStage::Inbound, judged);
        }
        ok
    }

    /// Defence in depth for an outbound connection that completed: the
    /// address actually connected must be allowed. The transport gate makes a
    /// refusal here unreachable in practice.
    pub(crate) fn outbound_established_allowed(&self, addr: &Multiaddr, peer: PeerId) -> bool {
        self.judge(PolicySource::Other, RefusalStage::Dial, addr, Some(peer))
    }

    /// Attribute the dial with this connection id to `source`.
    pub(crate) fn label_dial(&self, id: ConnectionId, source: PolicySource) {
        if self.allow.is_none() {
            return;
        }
        let mut d = lock(&self.dials);
        if d.labels.len() >= DIAL_LABEL_CAP {
            d.labels.clear();
        }
        d.labels.insert(id, source);
    }

    /// The swarm is starting the dial `id`: make its label current.
    pub(crate) fn begin_dial(&self, id: ConnectionId) {
        if self.allow.is_none() {
            return;
        }
        let mut d = lock(&self.dials);
        d.current = d.labels.remove(&id).unwrap_or(PolicySource::Other);
    }

    /// The dial `id` failed before it started; drop its label.
    pub(crate) fn forget_dial(&self, id: ConnectionId) {
        if self.allow.is_none() {
            return;
        }
        lock(&self.dials).labels.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ma(s: &str) -> Multiaddr {
        s.parse().expect("test multiaddr must parse")
    }

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    fn policy(entries: &[&str]) -> AddrPolicy {
        AddrPolicy::parse(entries)
            .expect("entries must parse")
            .expect("non-empty")
    }

    // ── Parsing ─────────────────────────────────────────────────────────

    #[test]
    fn empty_and_blank_lists_mean_no_policy() {
        assert_eq!(AddrPolicy::parse(Vec::<String>::new()).unwrap(), None);
        assert_eq!(AddrPolicy::parse(["", "  "]).unwrap(), None);
        // A trailing comma leaves a blank item; it is skipped, not an error.
        assert_eq!(policy(&["127.0.0.1", ""]).entries().len(), 1);
    }

    #[test]
    fn cidr_and_bare_ip_entries_parse_and_normalise() {
        let p = policy(&["127.0.0.1", "10.1.2.3/8", "::1", "2001:db8::1/32"]);
        let shown: Vec<String> = p.entries().iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            ["127.0.0.1/32", "10.0.0.0/8", "::1/128", "2001:db8::/32"]
        );
    }

    #[test]
    fn multiaddr_entries_parse() {
        let pid = peer();
        for ok in [
            "/ip4/127.0.0.1".to_string(),
            "/ip4/127.0.0.1/tcp/4001".into(),
            "/ip4/127.0.0.1/udp/4001".into(),
            "/ip4/127.0.0.1/udp/4001/quic-v1".into(),
            "/ip6/::1/tcp/4001".into(),
            "/dns4/relay.example.com/tcp/4001".into(),
            "/dnsaddr/bootstrap.example.com".into(),
            format!("/ip4/127.0.0.1/tcp/4001/p2p/{pid}"),
        ] {
            ok.parse::<AllowEntry>()
                .unwrap_or_else(|e| panic!("{ok} must parse: {e}"));
        }
    }

    #[test]
    fn malformed_entries_are_refused_with_the_flag_named() {
        let pid = peer();
        for bad in [
            "not-an-address".to_string(),
            "300.1.1.1".into(),
            "10.0.0.0/33".into(),
            "::1/129".into(),
            "10.0.0.0/x".into(),
            "/ip4/127.0.0.1/tcp/0".into(),
            "/ip4/127.0.0.1/tcp/4001/ws".into(),
            "/tcp/4001".into(),
            format!("/p2p/{pid}"),
            format!("/ip4/127.0.0.1/tcp/4001/p2p/{pid}/p2p-circuit"),
            "/ip4/127.0.0.1/tcp/4001/p2p-circuit".into(),
            "/ip4/10.0.0.0/ipcidr/8".into(),
            "/garbage".into(),
        ] {
            let err = bad
                .parse::<AllowEntry>()
                .expect_err(&format!("{bad} must be refused"));
            assert!(
                err.to_string().contains(OUTBOUND_ALLOW_FLAG),
                "error must name the flag: {err}"
            );
        }
    }

    #[test]
    fn a_peer_only_entry_explains_why_it_is_refused() {
        let err = format!("/p2p/{}", peer())
            .parse::<AllowEntry>()
            .unwrap_err();
        assert!(err.to_string().contains("peer ID alone"), "{err}");
    }

    // ── IP networks ─────────────────────────────────────────────────────

    #[test]
    fn cidr_matches_any_transport_and_port_inside_the_network() {
        let p = policy(&["10.0.0.0/8"]);
        assert!(p.allows(&ma("/ip4/10.1.2.3/tcp/4001"), None));
        assert!(p.allows(&ma("/ip4/10.255.0.1/udp/9/quic-v1"), None));
        assert!(p.allows(&ma(&format!("/ip4/10.0.0.1/tcp/1/p2p/{}", peer())), None));
        assert!(!p.allows(&ma("/ip4/192.0.2.1/tcp/4001"), None));

        // Boundaries, on a documentation range.
        let doc = policy(&["198.51.100.0/24"]);
        assert!(doc.allows(&ma("/ip4/198.51.100.0/tcp/1"), None));
        assert!(doc.allows(&ma("/ip4/198.51.100.255/tcp/1"), None));
        assert!(!doc.allows(&ma("/ip4/198.51.101.0/tcp/1"), None));
        assert!(!doc.allows(&ma("/ip4/198.51.99.255/tcp/1"), None));
    }

    #[test]
    fn single_host_entry_is_exact() {
        let p = policy(&["127.0.0.1"]);
        assert!(p.allows(&ma("/ip4/127.0.0.1/tcp/1"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.2/tcp/1"), None));
    }

    #[test]
    fn ipv6_networks_match_ipv6_and_mapped_ipv4_matches_ipv4_networks() {
        let p6 = policy(&["2001:db8::/32", "::1"]);
        assert!(p6.allows(&ma("/ip6/2001:db8::5/tcp/4001"), None));
        assert!(p6.allows(&ma("/ip6/::1/udp/4001/quic-v1"), None));
        assert!(!p6.allows(&ma("/ip6/2001:db9::5/tcp/4001"), None));
        assert!(!p6.allows(&ma("/ip4/127.0.0.1/tcp/4001"), None));

        let p4 = policy(&["192.0.2.0/24"]);
        assert!(p4.allows(&ma("/ip6/::ffff:192.0.2.7/tcp/4001"), None));
        assert!(!p4.allows(&ma("/ip6/::ffff:198.51.100.7/tcp/4001"), None));
    }

    #[test]
    fn ip_networks_never_match_dns_names() {
        let p = policy(&["0.0.0.0/0", "::/0"]);
        assert!(!p.allows(&ma("/dns4/relay.example.com/tcp/4001"), None));
        assert!(p.allows(&ma("/ip4/203.0.113.9/tcp/4001"), None));
    }

    // ── Multiaddr prefixes: TCP, QUIC, ports ────────────────────────────

    #[test]
    fn tcp_entry_matches_that_port_only_and_not_quic() {
        let p = policy(&["/ip4/127.0.0.1/tcp/4001"]);
        assert!(p.allows(&ma("/ip4/127.0.0.1/tcp/4001"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.1/tcp/4002"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.1/udp/4001/quic-v1"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.2/tcp/4001"), None));
    }

    #[test]
    fn quic_entry_matches_that_udp_port_only_and_not_tcp() {
        let p = policy(&["/ip4/127.0.0.1/udp/4001/quic-v1"]);
        assert!(p.allows(&ma("/ip4/127.0.0.1/udp/4001/quic-v1"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.1/udp/4002/quic-v1"), None));
        assert!(!p.allows(&ma("/ip4/127.0.0.1/tcp/4001"), None));
        // A bare UDP port is a prefix of the QUIC address on that port.
        let u = policy(&["/ip4/127.0.0.1/udp/4001"]);
        assert!(u.allows(&ma("/ip4/127.0.0.1/udp/4001/quic-v1"), None));
    }

    #[test]
    fn host_only_entry_allows_every_port_and_transport_on_the_host() {
        let p = policy(&["/ip6/::1"]);
        assert!(p.allows(&ma("/ip6/::1/tcp/1"), None));
        assert!(p.allows(&ma("/ip6/::1/udp/2/quic-v1"), None));
        assert!(!p.allows(&ma("/ip6/::2/tcp/1"), None));
    }

    #[test]
    fn dns_entries_match_by_name_case_insensitively_and_by_kind() {
        let p = policy(&["/dns4/Relay.Example.com/tcp/4001"]);
        assert!(p.allows(&ma("/dns4/relay.example.com/tcp/4001"), None));
        assert!(p.allows(&ma("/dns4/relay.example.com./tcp/4001"), None));
        assert!(!p.allows(&ma("/dns6/relay.example.com/tcp/4001"), None));
        assert!(!p.allows(&ma("/dns4/other.example.com/tcp/4001"), None));
        assert!(!p.allows(&ma("/dns4/relay.example.com/tcp/4002"), None));
    }

    // ── /p2p suffixes ───────────────────────────────────────────────────

    #[test]
    fn an_unpinned_entry_matches_any_peer_at_the_address() {
        let p = policy(&["/ip4/127.0.0.1/tcp/4001"]);
        assert!(p.allows(
            &ma(&format!("/ip4/127.0.0.1/tcp/4001/p2p/{}", peer())),
            None
        ));
    }

    #[test]
    fn a_pinned_entry_requires_the_pinned_peer() {
        let a = peer();
        let b = peer();
        let p = policy(&[&format!("/ip4/127.0.0.1/tcp/4001/p2p/{a}")]);
        assert!(p.allows(&ma(&format!("/ip4/127.0.0.1/tcp/4001/p2p/{a}")), None));
        assert!(!p.allows(&ma(&format!("/ip4/127.0.0.1/tcp/4001/p2p/{b}")), None));
        // No peer anywhere: the pin cannot be satisfied.
        assert!(!p.allows(&ma("/ip4/127.0.0.1/tcp/4001"), None));
        // The dial's target peer stands in for a missing /p2p suffix.
        assert!(p.allows(&ma("/ip4/127.0.0.1/tcp/4001"), Some(a)));
        assert!(!p.allows(&ma("/ip4/127.0.0.1/tcp/4001"), Some(b)));
    }

    // ── Relayed addresses ───────────────────────────────────────────────

    #[test]
    fn a_relayed_address_is_judged_by_its_relay_part() {
        let relay = peer();
        let dst = peer();
        let allowed = policy(&[&format!("/ip4/127.0.0.1/tcp/4001/p2p/{relay}")]);
        let via = ma(&format!(
            "/ip4/127.0.0.1/tcp/4001/p2p/{relay}/p2p-circuit/p2p/{dst}"
        ));
        assert!(allowed.allows(&via, None));
        // The destination does not matter, and a hint for it is ignored.
        assert!(allowed.allows(&via, Some(peer())));

        // Same relay address, different relay peer: refused.
        let other_relay = ma(&format!(
            "/ip4/127.0.0.1/tcp/4001/p2p/{}/p2p-circuit/p2p/{dst}",
            peer()
        ));
        assert!(!allowed.allows(&other_relay, None));

        // A relay at an address that is not allowed: refused, even though the
        // destination's own address would be.
        let p = policy(&["/ip4/127.0.0.1/tcp/5000"]);
        assert!(!p.allows(&via, None));
    }

    #[test]
    fn a_relayed_address_over_quic_is_judged_the_same_way() {
        let relay = peer();
        let p = policy(&["/ip4/127.0.0.1/udp/4001/quic-v1"]);
        let via = ma(&format!(
            "/ip4/127.0.0.1/udp/4001/quic-v1/p2p/{relay}/p2p-circuit/p2p/{}",
            peer()
        ));
        assert!(p.allows(&via, None));
    }

    #[test]
    fn a_relay_part_without_a_host_matches_nothing() {
        let relay = peer();
        let p = policy(&["0.0.0.0/0", "::/0", "/ip4/127.0.0.1"]);
        assert!(!p.allows(
            &ma(&format!("/p2p/{relay}/p2p-circuit/p2p/{}", peer())),
            None
        ));
    }

    #[test]
    fn non_host_addresses_are_never_allowed() {
        let p = policy(&["0.0.0.0/0", "::/0"]);
        assert!(!p.allows(&ma("/memory/1234"), None));
        assert!(!p.allows(&ma("/unix/tmp%2Fsock"), None));
        assert!(!p.allows(&ma("/ip6zone/eth0/ip6/fe80::1/tcp/1"), None));
        assert!(!p.allows(&Multiaddr::empty(), None));
    }

    // ── Runtime state ───────────────────────────────────────────────────

    #[test]
    fn without_a_policy_nothing_is_refused_or_counted() {
        let np = NetPolicy::new(true, None);
        assert!(!np.is_restricted());
        assert!(np.admit(PolicySource::Identify, &ma("/ip4/203.0.113.9/tcp/1"), None));
        assert!(
            np.check_transport_dial(&ma("/memory/1"), Endpoint::Listener)
                .is_ok()
        );
        assert!(np.inbound_allowed(
            &ma("/ip4/0.0.0.0/tcp/1"),
            &ma("/ip4/203.0.113.9/tcp/9"),
            None
        ));
        assert_eq!(np.refusals(), PolicyRefusals::default());
        assert_eq!(np.describe(), "mdns=on outbound_allow=unrestricted");
    }

    #[test]
    fn refusals_are_counted_by_source_and_audited() {
        let np = NetPolicy::new(false, Some(policy(&["/ip4/127.0.0.1/tcp/4001"])));
        let mut rx = np.subscribe();
        let bad = ma("/ip4/127.0.0.1/udp/4001/quic-v1");

        assert!(!np.admit(PolicySource::Mdns, &bad, None));
        assert!(!np.admit(PolicySource::Mdns, &bad, None));
        assert!(!np.admit_dial(PolicySource::Kad, &bad, None));
        assert!(np.admit(PolicySource::Identify, &ma("/ip4/127.0.0.1/tcp/4001"), None));

        let r = np.refusals();
        assert_eq!((r.mdns, r.kad, r.identify, r.total()), (2, 1, 0, 3));
        assert_eq!(
            rx.try_recv().unwrap(),
            ConnectionAudit::Refused {
                source: PolicySource::Mdns,
                stage: RefusalStage::Admission,
                addr: bad.clone()
            }
        );
        assert!(
            np.describe()
                .starts_with("mdns=off outbound_allow=[/ip4/127.0.0.1/tcp/4001]")
        );
    }

    #[test]
    fn transport_refusals_take_the_label_of_the_dial_in_progress() {
        let np = NetPolicy::new(true, Some(policy(&["/ip4/127.0.0.1/tcp/4001"])));
        let bad = ma("/ip4/127.0.0.1/tcp/5000");

        let id = ConnectionId::new_unchecked(7);
        np.label_dial(id, PolicySource::Bootstrap);
        np.begin_dial(id);
        assert_eq!(
            np.check_transport_dial(&bad, Endpoint::Dialer),
            Err(PolicySource::Bootstrap)
        );

        // An unlabelled dial is `other`, unless it is a hole-punch.
        np.begin_dial(ConnectionId::new_unchecked(8));
        assert_eq!(
            np.check_transport_dial(&bad, Endpoint::Dialer),
            Err(PolicySource::Other)
        );
        assert_eq!(
            np.check_transport_dial(&bad, Endpoint::Listener),
            Err(PolicySource::Autodial)
        );
        assert!(
            np.check_transport_dial(&ma("/ip4/127.0.0.1/tcp/4001"), Endpoint::Dialer)
                .is_ok()
        );

        let r = np.refusals();
        assert_eq!((r.bootstrap, r.other, r.autodial), (1, 1, 1));
    }

    #[test]
    fn inbound_direct_is_judged_by_remote_and_relayed_by_relay() {
        let relay = peer();
        let src = peer();
        let np = NetPolicy::new(
            true,
            Some(policy(&[&format!("/ip4/127.0.0.1/tcp/4001/p2p/{relay}")])),
        );
        let local = ma("/ip4/0.0.0.0/udp/9/quic-v1");

        // Direct inbound from an address that is not allowed.
        assert!(!np.inbound_allowed(&local, &ma("/ip4/127.0.0.1/udp/7000/quic-v1"), None));
        // Direct inbound from the relay's address: allowed only once the peer
        // is known to be the pinned relay.
        let from_relay = ma("/ip4/127.0.0.1/tcp/4001");
        assert!(np.inbound_allowed(&local, &from_relay, None), "deferred");
        assert!(np.inbound_allowed(&local, &from_relay, Some(relay)));
        assert!(!np.inbound_allowed(&local, &from_relay, Some(src)));

        // Relayed inbound through the allowed relay, and through another one.
        let circuit = ma(&format!("/ip4/127.0.0.1/tcp/4001/p2p/{relay}/p2p-circuit"));
        assert!(np.inbound_allowed(&circuit, &ma(&format!("/p2p/{src}")), Some(src)));
        let other = ma(&format!("/ip4/127.0.0.1/tcp/4001/p2p/{src}/p2p-circuit"));
        assert!(!np.inbound_allowed(&other, &ma(&format!("/p2p/{relay}")), Some(relay)));

        assert_eq!(np.refusals().inbound, 3);
    }
}
