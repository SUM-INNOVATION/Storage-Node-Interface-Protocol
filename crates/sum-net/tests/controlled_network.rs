//! Controlled-network mode (SNIP #79), observed over real swarms on loopback.
//!
//! Every node binds 127.0.0.1 only and runs with mDNS off. libp2p-mdns skips
//! loopback interfaces, so a live multicast test cannot stay on loopback;
//! "mDNS off" is pinned structurally here (no mDNS behaviour is built) and
//! mDNS admission is pinned by the handler tests in `src/discovery.rs`.
//!
//! "Archives" are `SumNet` nodes with a responder that answers every V2
//! request; "upload" is a V2 `Push` answered by `PushAck`, "download" a V2
//! `Pull` answered by `Data`. The allowed endpoint is a TCP forwarder in
//! front of an archive — the shape of an application-level relay — so that
//! interrupting and restoring it is a matter of closing and reopening one
//! port.
//!
//! Route assertions are made on the connection audit stream, which carries
//! the swarm's own `ConnectionEstablished` / `ConnectionClosed` events with
//! the remote address, and on the raw swarm's `SwarmEvent`s where a test
//! drives one directly — never on log text.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::swarm::{ConnectionId, DialError, SwarmEvent};
use libp2p::{Multiaddr, PeerId, TransportError};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use sum_net::swarm::build_swarm;
use sum_net::{
    ConnectionAudit, Keypair, NetPolicy, PolicySource, RefusalStage, ShardRequestV2,
    ShardResponseV2, SumNet, SumNetEvent,
};
use sum_types::config::NetConfig;

/// Budget for any one wait. Loopback settles in milliseconds; this bounds a
/// hang.
const STEP: Duration = Duration::from_secs(20);

const PAYLOAD: &[u8] = b"snip-79 controlled-network test chunk";
const CID: &str = "bafk-controlled-network-test";
const ROOT: [u8; 32] = [7; 32];

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Logs go to the test harness only when `RUST_LOG` is set, e.g.
/// `RUST_LOG=sum_net=info cargo test -p sum-net --test controlled_network -- --nocapture`.
fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
}

fn config(allow: &[String], bootstrap: &[String]) -> NetConfig {
    NetConfig {
        enable_wan: true,
        mdns: false,
        listen_ip: Some(Ipv4Addr::LOCALHOST),
        bootstrap_peers: bootstrap.to_vec(),
        outbound_allow: allow.to_vec(),
        ..NetConfig::default()
    }
}

fn with_p2p(addr: &Multiaddr, peer: PeerId) -> Multiaddr {
    addr.clone().with(Protocol::P2p(peer))
}

fn is_quic(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::QuicV1))
}

fn tcp_socket(addr: &Multiaddr) -> SocketAddr {
    let mut ip = None;
    let mut port = None;
    for p in addr.iter() {
        match p {
            Protocol::Ip4(i) => ip = Some(i),
            Protocol::Tcp(t) => port = Some(t),
            _ => {}
        }
    }
    SocketAddr::from((ip.expect("ip4"), port.expect("tcp")))
}

/// Strip a trailing `/p2p/<peer>` for prefix comparisons.
fn starts_with(addr: &Multiaddr, prefix: &Multiaddr) -> bool {
    let a: Vec<_> = addr.iter().collect();
    let p: Vec<_> = prefix.iter().collect();
    a.len() >= p.len() && a[..p.len()] == p[..]
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + STEP;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn respond(net: &SumNet, channel_id: u64, request: ShardRequestV2) {
    let response = match request {
        ShardRequestV2::Pull { cid, offset, .. } => ShardResponseV2::Data {
            cid,
            offset,
            total_bytes: PAYLOAD.len() as u64,
            data: PAYLOAD.to_vec(),
            error: None,
        },
        ShardRequestV2::Push {
            merkle_root,
            chunk_index,
            ..
        } => ShardResponseV2::PushAck {
            merkle_root,
            chunk_index,
            error: None,
        },
        ShardRequestV2::ManifestPush { merkle_root, .. } => ShardResponseV2::ManifestPushAck {
            merkle_root,
            error: None,
        },
        ShardRequestV2::ManifestPull { merkle_root } => ShardResponseV2::ManifestData {
            merkle_root,
            manifest_bytes: Vec::new(),
            error: None,
        },
    };
    net.respond_shard_v2(channel_id, response)
        .await
        .expect("respond");
}

/// A running `SumNet` node with its event pump and audit history.
struct Node {
    net: Arc<SumNet>,
    peer: PeerId,
    events: mpsc::UnboundedReceiver<SumNetEvent>,
    backlog: VecDeque<SumNetEvent>,
    audit_rx: broadcast::Receiver<ConnectionAudit>,
    audit: Vec<ConnectionAudit>,
    listen: Vec<Multiaddr>,
    pump: JoinHandle<()>,
}

impl Node {
    /// Start a node. `serve` makes it an archive that answers V2 requests.
    async fn start(cfg: NetConfig, serve: bool) -> Self {
        init_tracing();
        let kp = Keypair::generate_ed25519();
        let peer = kp.public().to_peer_id();
        let net = Arc::new(SumNet::new(cfg, kp).await.expect("SumNet::new"));
        let audit_rx = net.connection_audit();
        let (tx, events) = mpsc::unbounded_channel();
        let pump_net = net.clone();
        let pump = tokio::spawn(async move {
            while let Some(ev) = pump_net.next_event().await {
                if serve
                    && let SumNetEvent::ShardRequestedV2 {
                        channel_id,
                        request,
                        ..
                    } = &ev
                {
                    respond(&pump_net, *channel_id, request.clone()).await;
                    continue;
                }
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
        let mut node = Self {
            net,
            peer,
            events,
            backlog: VecDeque::new(),
            audit_rx,
            audit: Vec::new(),
            listen: Vec::new(),
            pump,
        };
        while node.listen.len() < 2 {
            let addr = node
                .wait_for("a listen address", |ev| match ev {
                    SumNetEvent::Listening { addr } => Some(addr.clone()),
                    _ => None,
                })
                .await;
            node.listen.push(addr);
        }
        node
    }

    fn quic_addr(&self) -> Multiaddr {
        self.listen
            .iter()
            .find(|a| is_quic(a))
            .cloned()
            .expect("QUIC listener")
    }

    fn tcp_addr(&self) -> Multiaddr {
        self.listen
            .iter()
            .find(|a| !is_quic(a))
            .cloned()
            .expect("TCP listener")
    }

    /// Wait for the first event `pick` accepts; others are kept for later.
    async fn wait_for<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&SumNetEvent) -> Option<T>,
    ) -> T {
        if let Some(i) = self.backlog.iter().position(|e| pick(e).is_some()) {
            let ev = self.backlog.remove(i).expect("indexed");
            return pick(&ev).expect("matched above");
        }
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = timeout(left, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .expect("event pump ended");
            if let Some(t) = pick(&ev) {
                return t;
            }
            self.backlog.push_back(ev);
        }
    }

    async fn wait_identified(&mut self, peer: PeerId) {
        self.wait_for("PeerIdentified", |ev| match ev {
            SumNetEvent::PeerIdentified { peer_id, .. } if *peer_id == peer => Some(()),
            _ => None,
        })
        .await
    }

    async fn wait_disconnected(&mut self, peer: PeerId) {
        self.wait_for("PeerDisconnected", |ev| match ev {
            SumNetEvent::PeerDisconnected { peer_id } if *peer_id == peer => Some(()),
            _ => None,
        })
        .await
    }

    /// V2 Pull from `peer`. `Err` carries the swarm's failure text.
    async fn pull(&mut self, peer: PeerId) -> Result<Vec<u8>, String> {
        self.net
            .pull_chunk_v2(peer, CID.to_string(), 0, 4096)
            .await
            .expect("send pull");
        self.wait_for("pull outcome", |ev| match ev {
            SumNetEvent::ShardReceivedV2 {
                peer_id,
                response: ShardResponseV2::Data { data, .. },
                ..
            } if *peer_id == peer => Some(Ok(data.clone())),
            SumNetEvent::ShardRequestFailed { peer_id, error, .. } if *peer_id == peer => {
                Some(Err(error.clone()))
            }
            _ => None,
        })
        .await
    }

    /// V2 Push to `peer`.
    async fn push(&mut self, peer: PeerId) -> Result<(), String> {
        self.net
            .push_chunk_v2(peer, PAYLOAD.to_vec(), ROOT, 0, Vec::new())
            .await
            .expect("send push");
        self.wait_for("push outcome", |ev| match ev {
            SumNetEvent::ShardReceivedV2 {
                peer_id,
                response: ShardResponseV2::PushAck { error: None, .. },
                ..
            } if *peer_id == peer => Some(Ok(())),
            SumNetEvent::ShardRequestFailed { peer_id, error, .. } if *peer_id == peer => {
                Some(Err(error.clone()))
            }
            _ => None,
        })
        .await
    }

    /// Everything the audit stream has delivered so far.
    fn audit(&mut self) -> &[ConnectionAudit] {
        loop {
            match self.audit_rx.try_recv() {
                Ok(ev) => self.audit.push(ev),
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    panic!("audit receiver lagged by {n}; raise the capacity or drain sooner")
                }
                Err(_) => break,
            }
        }
        &self.audit
    }

    /// `(peer, remote address)` of every connection established so far.
    fn established(&mut self) -> Vec<(PeerId, Multiaddr)> {
        self.audit()
            .iter()
            .filter_map(|e| match e {
                ConnectionAudit::Established {
                    peer_id,
                    remote_addr,
                    ..
                } => Some((*peer_id, remote_addr.clone())),
                _ => None,
            })
            .collect()
    }

    fn refused(&mut self, source: PolicySource) -> Vec<(RefusalStage, Multiaddr)> {
        self.audit()
            .iter()
            .filter_map(|e| match e {
                ConnectionAudit::Refused {
                    source: s,
                    stage,
                    addr,
                } if *s == source => Some((*stage, addr.clone())),
                _ => None,
            })
            .collect()
    }

    async fn stop(self) {
        let _ = self.net.shutdown().await;
        self.pump.abort();
    }
}

/// A TCP forwarder: the allowed endpoint, in front of an archive's TCP
/// listener. Stopping it closes the port and every forwarded connection.
struct Forwarder {
    port: u16,
    accept: JoinHandle<()>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Forwarder {
    async fn start(port: u16, target: SocketAddr) -> Self {
        let deadline = Instant::now() + STEP;
        let listener = loop {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(l) => break l,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("forwarder bind on {port}: {e}"),
            }
        };
        let port = listener.local_addr().expect("local addr").port();
        let conns = Arc::new(Mutex::new(Vec::new()));
        let tracked = conns.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let task = tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                tracked.lock().unwrap().push(task);
            }
        });
        Self {
            port,
            accept,
            conns,
        }
    }

    fn addr(&self) -> Multiaddr {
        format!("/ip4/127.0.0.1/tcp/{}", self.port)
            .parse()
            .expect("forwarder multiaddr")
    }

    /// Close the port and every connection through it; returns the port.
    fn stop(self) -> u16 {
        self.accept.abort();
        for t in self.conns.lock().unwrap().drain(..) {
            t.abort();
        }
        self.port
    }
}

// ── A raw production swarm, driven by the test ───────────────────────────────

#[derive(Debug)]
enum RawEvent {
    Listening(Multiaddr),
    Established { id: ConnectionId, peer: PeerId },
    Closed { id: ConnectionId },
    DialFailed { id: ConnectionId, error: String },
}

/// The production swarm (`build_swarm`), run in a task the test can command.
struct RawNode {
    peer: PeerId,
    dial_tx: mpsc::UnboundedSender<(DialOpts, oneshot::Sender<ConnectionId>)>,
    events: mpsc::UnboundedReceiver<RawEvent>,
    task: JoinHandle<()>,
}

impl RawNode {
    fn spawn(cfg: NetConfig, policy: Arc<NetPolicy>, listen: &[&str]) -> Self {
        let kp = Keypair::generate_ed25519();
        let peer = kp.public().to_peer_id();
        let mut swarm = build_swarm(&cfg, kp, policy).expect("build_swarm");
        for l in listen {
            swarm.listen_on(l.parse().unwrap()).expect("listen");
        }
        let (dial_tx, mut dial_rx) =
            mpsc::unbounded_channel::<(DialOpts, oneshot::Sender<ConnectionId>)>();
        let (tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    ev = swarm.select_next_some() => {
                        let out = match ev {
                            SwarmEvent::NewListenAddr { address, .. } => Some(RawEvent::Listening(address)),
                            SwarmEvent::ConnectionEstablished { connection_id, peer_id, .. } => {
                                Some(RawEvent::Established { id: connection_id, peer: peer_id })
                            }
                            SwarmEvent::ConnectionClosed { connection_id, .. } => {
                                Some(RawEvent::Closed { id: connection_id })
                            }
                            SwarmEvent::OutgoingConnectionError { connection_id, error, .. } => {
                                Some(RawEvent::DialFailed { id: connection_id, error: describe_dial_error(&error) })
                            }
                            _ => None,
                        };
                        if let Some(out) = out && tx.send(out).is_err() {
                            break;
                        }
                    }
                    cmd = dial_rx.recv() => {
                        let Some((opts, reply)) = cmd else { break };
                        let id = opts.connection_id();
                        let _ = swarm.dial(opts);
                        let _ = reply.send(id);
                    }
                }
            }
        });
        Self {
            peer,
            dial_tx,
            events,
            task,
        }
    }

    async fn dial(&self, opts: DialOpts) -> ConnectionId {
        let (tx, rx) = oneshot::channel();
        self.dial_tx.send((opts, tx)).expect("raw node running");
        rx.await.expect("dial id")
    }

    async fn next(&mut self, what: &str, mut pick: impl FnMut(&RawEvent) -> bool) -> RawEvent {
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = timeout(left, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .expect("raw node ended");
            if pick(&ev) {
                return ev;
            }
        }
    }
}

/// Flatten a dial error to text that keeps each refused address's reason.
fn describe_dial_error(error: &DialError) -> String {
    match error {
        DialError::Transport(errs) => errs
            .iter()
            .map(|(addr, e)| match e {
                TransportError::Other(io) => format!("{addr}: {:?}: {io}", io.kind()),
                TransportError::MultiaddrNotSupported(a) => format!("{addr}: unsupported {a}"),
            })
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// (b) uploads and downloads succeed through the only allowed endpoint;
/// (c) an allowed archive advertising its direct QUIC and TCP addresses
///     through Identify, and Kademlia (through that archive) offering a second
///     archive's direct addresses, produce no connection to any of them;
/// and the issue's acceptance sequence: interrupting the endpoint stops the
/// restricted client's transfers while an unrestricted control client still
/// reaches the archive, and restoring the endpoint restores them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_controlled_client_routes_only_through_the_allowed_endpoint() {
    let mut a = Node::start(config(&[], &[]), true).await;
    // B joins through A, so A's Kademlia table holds B's direct addresses.
    let mut b = Node::start(
        config(&[], &[with_p2p(&a.tcp_addr(), a.peer).to_string()]),
        true,
    )
    .await;
    b.wait_identified(a.peer).await;
    a.wait_identified(b.peer).await;

    let fwd = Forwarder::start(0, tcp_socket(&a.tcp_addr())).await;
    let route = fwd.addr();
    let mut c = Node::start(
        config(
            &[route.to_string()],
            &[with_p2p(&route, a.peer).to_string()],
        ),
        false,
    )
    .await;
    c.wait_identified(a.peer).await;

    // Upload and download through the allowed endpoint.
    c.push(a.peer)
        .await
        .expect("push through the allowed endpoint");
    assert_eq!(
        c.pull(a.peer)
            .await
            .expect("pull through the allowed endpoint"),
        PAYLOAD
    );

    // A's direct addresses arrived through Identify and were refused; B's
    // arrived through Kademlia (A's answer to C's bootstrap query) and were
    // refused when Kademlia offered them for a dial.
    let net = c.net.clone();
    wait_until("identify and kad refusals", || {
        let r = net.net_policy_refusals();
        r.identify >= 2 && r.kad >= 1
    })
    .await;
    let identify = c.refused(PolicySource::Identify);
    for alt in [a.quic_addr(), a.tcp_addr()] {
        assert!(
            identify
                .iter()
                .any(|(stage, addr)| *stage == RefusalStage::Admission && *addr == alt),
            "A's alternative {alt} must be refused at admission: {identify:?}"
        );
    }
    let kad = c.refused(PolicySource::Kad);
    assert!(
        kad.iter().any(|(_, addr)| {
            starts_with(addr, &b.quic_addr()) || starts_with(addr, &b.tcp_addr())
        }),
        "B's direct addresses must be refused as Kademlia dial targets: {kad:?}"
    );

    // From the swarm's connection events: every connection C made went
    // through the forwarder, to A; none to B, none to A's direct addresses.
    let routes = c.established();
    assert!(!routes.is_empty());
    for (peer, remote) in &routes {
        assert_eq!(*peer, a.peer, "only A is reachable: {routes:?}");
        assert!(
            starts_with(remote, &route),
            "connection off the route: {remote}"
        );
    }

    // Interrupt the single configured path.
    let port = fwd.stop();
    c.wait_disconnected(a.peer).await;
    let before = c.established().len();

    // An unrestricted control client still reaches the archive directly.
    let mut d = Node::start(
        config(&[], &[with_p2p(&a.quic_addr(), a.peer).to_string()]),
        false,
    )
    .await;
    d.wait_identified(a.peer).await;
    assert_eq!(d.pull(a.peer).await.expect("control client pull"), PAYLOAD);
    assert_eq!(d.net.net_policy_refusals().total(), 0);

    // The restricted client cannot transfer, and opens no other route.
    let err = c
        .pull(a.peer)
        .await
        .expect_err("no transfer while the endpoint is down");
    assert!(!err.is_empty());
    assert_eq!(
        c.established().len(),
        before,
        "no new route while interrupted"
    );

    // Restoring the path restores transfers, through the same endpoint.
    let fwd = Forwarder::start(port, tcp_socket(&a.tcp_addr())).await;
    assert_eq!(
        c.pull(a.peer)
            .await
            .expect("pull after the endpoint is restored"),
        PAYLOAD
    );
    let routes = c.established();
    assert!(routes.len() > before, "a new connection after restore");
    for (peer, remote) in &routes {
        assert_eq!(*peer, a.peer);
        assert!(
            starts_with(remote, &route),
            "connection off the route: {remote}"
        );
    }

    fwd.stop();
    for n in [a, b, c, d] {
        n.stop().await;
    }
}

/// (c, inbound) An allowed peer that dials back to the restricted client's
/// direct QUIC address — what DCUtR, its direct-dial shortcut or Kademlia
/// would do — gets no second route: the client refuses the connection, counts
/// it, and keeps only the route through the allowed endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allowed_peer_cannot_open_a_second_route_by_dialing_in() {
    let mut x = RawNode::spawn(
        config(&[], &[]),
        Arc::new(NetPolicy::new(false, None)),
        &["/ip4/127.0.0.1/tcp/0", "/ip4/127.0.0.1/udp/0/quic-v1"],
    );
    let mut x_listen = Vec::new();
    while x_listen.len() < 2 {
        if let RawEvent::Listening(a) = x
            .next("raw listen address", |e| {
                matches!(e, RawEvent::Listening(_))
            })
            .await
        {
            x_listen.push(a);
        }
    }
    let x_tcp = x_listen.iter().find(|a| !is_quic(a)).cloned().unwrap();
    let x_quic = x_listen.iter().find(|a| is_quic(a)).cloned().unwrap();

    let fwd = Forwarder::start(0, tcp_socket(&x_tcp)).await;
    let route = fwd.addr();
    let mut c = Node::start(
        config(
            &[route.to_string()],
            &[with_p2p(&route, x.peer).to_string()],
        ),
        false,
    )
    .await;
    let first = match x
        .next("C's connection through the forwarder", |e| {
            matches!(e, RawEvent::Established { .. })
        })
        .await
    {
        RawEvent::Established { id, peer } => {
            assert_eq!(peer, c.peer);
            id
        }
        _ => unreachable!(),
    };
    c.wait_identified(x.peer).await;

    // X opens a second, direct QUIC connection to C.
    let id = x
        .dial(
            DialOpts::peer_id(c.peer)
                .condition(PeerCondition::Always)
                .addresses(vec![with_p2p(&c.quic_addr(), c.peer)])
                .build(),
        )
        .await;
    let outcome = x
        .next("the direct dial's outcome", |e| match e {
            RawEvent::DialFailed { id: i, .. } | RawEvent::Closed { id: i } => *i == id,
            _ => false,
        })
        .await;
    assert!(
        matches!(
            outcome,
            RawEvent::DialFailed { .. } | RawEvent::Closed { .. }
        ),
        "{outcome:?}"
    );

    let net = c.net.clone();
    wait_until("an inbound refusal", || {
        net.net_policy_refusals().inbound >= 1
    })
    .await;
    let inbound = c.refused(PolicySource::Inbound);
    assert!(
        inbound
            .iter()
            .any(|(stage, addr)| *stage == RefusalStage::Inbound && starts_with(addr, &x_quic)),
        "the direct QUIC connection from {x_quic} must be refused as inbound: {inbound:?}"
    );
    // C's connections, from its swarm's events: only through the forwarder
    // (a bootstrap dial and a Kademlia dial may race to open two there), and
    // none over QUIC.
    let routes = c.established();
    assert!(!routes.is_empty());
    for (peer, remote) in &routes {
        assert_eq!(*peer, x.peer, "{routes:?}");
        assert!(
            starts_with(remote, &route) && !is_quic(remote),
            "only the forwarded route may exist: {routes:?}"
        );
    }
    // The route through the endpoint is still up on X's side.
    while let Ok(ev) = x.events.try_recv() {
        assert!(
            !matches!(ev, RawEvent::Closed { id } if id == first),
            "the forwarded connection must survive"
        );
    }

    fwd.stop();
    c.stop().await;
    x.task.abort();
}

/// (f) A QUIC-only allow-list routes over QUIC: the transfer succeeds over
/// the allowed QUIC address and the archive's TCP address, advertised through
/// Identify, is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quic_only_allow_list_routes_over_quic() {
    let a = Node::start(config(&[], &[]), true).await;
    let quic = a.quic_addr();
    let mut e = Node::start(
        config(&[quic.to_string()], &[with_p2p(&quic, a.peer).to_string()]),
        false,
    )
    .await;
    e.wait_identified(a.peer).await;
    e.push(a.peer).await.expect("push over QUIC");
    assert_eq!(e.pull(a.peer).await.expect("pull over QUIC"), PAYLOAD);

    let net = e.net.clone();
    wait_until("identify refusal of the TCP address", || {
        net.net_policy_refusals().identify >= 1
    })
    .await;
    let tcp = a.tcp_addr();
    assert!(
        e.refused(PolicySource::Identify)
            .iter()
            .any(|(_, addr)| *addr == tcp),
        "A's TCP address must be refused"
    );
    let routes = e.established();
    assert!(!routes.is_empty());
    for (peer, remote) in &routes {
        assert_eq!(*peer, a.peer);
        assert!(
            is_quic(remote) && starts_with(remote, &quic),
            "QUIC route only: {remote}"
        );
    }
    e.stop().await;
    a.stop().await;
}

/// (d, bootstrap) A bootstrap peer outside the allow-list is refused at
/// admission, counted, never dialed, and does not fail startup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disallowed_bootstrap_peer_is_refused_and_counted() {
    let mut a = Node::start(config(&[], &[]), true).await;
    let target = with_p2p(&a.quic_addr(), a.peer);
    let mut g = Node::start(
        config(&["/ip4/127.0.0.1/tcp/9".into()], &[target.to_string()]),
        false,
    )
    .await;

    assert_eq!(g.net.net_policy_refusals().bootstrap, 1);
    assert_eq!(
        g.refused(PolicySource::Bootstrap),
        vec![(RefusalStage::Admission, target.clone())]
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(g.established().is_empty(), "nothing may be dialed");
    assert!(
        !a.established().iter().any(|(p, _)| *p == g.peer),
        "the archive must see no connection from the client"
    );
    g.stop().await;
    a.stop().await;
}

/// (d, autodial; f) Dials refused at the transport gate, over QUIC, to an
/// address with a live listener behind it: a hole-punch (listener-role) dial
/// is counted as `autodial`, an unlabelled dialer-role dial as `other`, and
/// neither produces a connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dials_to_a_disallowed_quic_address_are_refused_at_the_transport() {
    let mut a = Node::start(config(&[], &[]), true).await;
    let quic = with_p2p(&a.quic_addr(), a.peer);
    let policy = Arc::new(NetPolicy::new(
        false,
        sum_net::AddrPolicy::parse(["/ip4/127.0.0.1/tcp/9"]).unwrap(),
    ));
    let mut s = RawNode::spawn(config(&[], &[]), policy.clone(), &[]);

    for (opts, source) in [
        (
            DialOpts::peer_id(a.peer)
                .condition(PeerCondition::Always)
                .addresses(vec![quic.clone()])
                .override_role()
                .build(),
            PolicySource::Autodial,
        ),
        (
            DialOpts::peer_id(a.peer)
                .condition(PeerCondition::Always)
                .addresses(vec![quic.clone()])
                .build(),
            PolicySource::Other,
        ),
    ] {
        let before = policy.refusals().get(source);
        let id = s.dial(opts).await;
        match s
            .next("the refused dial", |e| matches!(e, RawEvent::DialFailed { id: i, .. } | RawEvent::Established { id: i, .. } if *i == id))
            .await
        {
            RawEvent::DialFailed { error, .. } => {
                assert!(
                    error.contains("refused by the --outbound-allow policy"),
                    "{error}"
                );
                assert!(error.contains(source.label()), "{error}");
            }
            other => panic!("the dial must be refused, got {other:?}"),
        }
        assert_eq!(policy.refusals().get(source), before + 1, "{source:?}");
    }
    assert!(
        !a.established().iter().any(|(p, _)| *p == s.peer),
        "the archive must see no connection"
    );
    s.task.abort();
    a.stop().await;
}

/// (e) With no policy the node behaves as before: mDNS is built, nothing is
/// refused, Identify addresses are admitted, transfers work.
/// (a) With mDNS off, no mDNS behaviour exists at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defaults_are_unchanged_and_mdns_off_builds_no_mdns() {
    // Structural: the default config builds mDNS and an unrestricted policy.
    let default_policy = Arc::new(NetPolicy::from_config(&NetConfig::default()).unwrap());
    assert!(!default_policy.is_restricted());
    assert_eq!(
        default_policy.describe(),
        "mdns=on outbound_allow=unrestricted"
    );
    let swarm = build_swarm(
        &NetConfig::default(),
        Keypair::generate_ed25519(),
        default_policy,
    )
    .unwrap();
    assert!(swarm.behaviour().mdns.is_enabled(), "mDNS is on by default");
    drop(swarm);

    let off = NetConfig {
        mdns: false,
        ..NetConfig::default()
    };
    let swarm = build_swarm(
        &off,
        Keypair::generate_ed25519(),
        Arc::new(NetPolicy::from_config(&off).unwrap()),
    )
    .unwrap();
    assert!(
        !swarm.behaviour().mdns.is_enabled(),
        "--mdns off builds no mDNS behaviour"
    );
    drop(swarm);

    // Live: two unrestricted nodes, direct QUIC, nothing refused.
    let mut a = Node::start(config(&[], &[]), true).await;
    let mut n = Node::start(
        config(&[], &[with_p2p(&a.quic_addr(), a.peer).to_string()]),
        false,
    )
    .await;
    n.wait_identified(a.peer).await;
    a.wait_identified(n.peer).await;
    n.push(a.peer).await.expect("push");
    assert_eq!(n.pull(a.peer).await.expect("pull"), PAYLOAD);
    assert_eq!(n.net.net_policy_refusals().total(), 0);
    assert_eq!(a.net.net_policy_refusals().total(), 0);
    assert!(n.refused(PolicySource::Identify).is_empty());
    assert!(
        n.established()
            .iter()
            .any(|(p, remote)| *p == a.peer && starts_with(remote, &a.quic_addr()))
    );
    // Without a policy no `Refused` event is ever emitted.
    assert!(
        !n.audit()
            .iter()
            .chain(a.audit().iter())
            .any(|e| matches!(e, ConnectionAudit::Refused { .. }))
    );
    n.stop().await;
    a.stop().await;
}
