# Networking

SNIP archives and clients discover each other over libp2p. This
document describes the current-day networking surface: which
discovery mechanisms run when, which transports SNIP uses, and how
NAT traversal works for archives that are not directly reachable.
Historical narrative on how this shipped is preserved in
[`../archive/WAN-DISCOVERY-AND-HARDENING.md`](../archive/WAN-DISCOVERY-AND-HARDENING.md).

Configuration flags that toggle networking behavior are described
in [`../reference/config-flags.md`](../reference/config-flags.md).

## Discovery

Two discovery mechanisms run in parallel. Which are active depends
on `--enable-wan`.

- **mDNS** (LAN, on by default; `--mdns off` disables it — see
  [Controlled networks](#controlled-networks)). Multicast DNS on the
  local link. Peers on the same LAN discover each other automatically
  without bootstrap. Suitable for local-mirror testing, LAN staging,
  and single-site deployments.
- **Kademlia DHT** (WAN, gated by `--enable-wan`). Distributed hash
  table for internet-wide peer discovery. Requires at least one
  `--bootstrap-peer` multiaddr to seed the routing table. Peers
  learn each other's listen addresses through Kademlia queries and
  through libp2p's `identify` protocol.

When both are active, discovery events feed the same downstream
consumer (`swarm::handle_*_event` in `sum-net`) — the mesh sees no
difference between LAN and WAN peers once discovered.

Kademlia protocol ID: `/sum/kad/1.0.0`. Kademlia bootstrap is
initiated from `SumNet::new` (in `crates/sum-net/src/lib.rs`) when
`enable_wan` is true; the parsing + dial logic is in
`crates/sum-net/src/swarm.rs::bootstrap_kademlia`.

## Transports

- **QUIC/UDP** — always on. `/ip4/0.0.0.0/udp/{udp_port}/quic-v1`.
  Preferred wherever it works. Pinning `--udp-port` matters when
  NAT traversal is in play (DCUtR needs a stable UDP hole-punch
  target on restart).
- **TCP + Noise + Yamux** — added when `--enable-wan` is on. Some
  NATs and firewalls block UDP; TCP is the fallback transport
  for those environments. `/ip4/0.0.0.0/tcp/{tcp_port}`. Uses
  Noise for encryption and Yamux for stream multiplexing.

Both transports use the same libp2p peer ID and address the same
mesh; the choice of transport is per-connection.

## NAT traversal

Archives that are not directly reachable from the public internet
join the mesh via libp2p's relay + hole-punch stack.

- **Circuit Relay v2 client** — always enabled. An archive behind
  NAT can reserve a slot on any publicly-reachable peer running
  `--relay-server`, and other peers can dial it through the relay.
- **Circuit Relay v2 server** — gated by `--relay-server`. Only
  enable on publicly-reachable hosts (VPS with a stable IP,
  port-forwarded home server, etc.). Requires `--enable-wan`;
  without WAN the server is unreachable to anyone.
- **DCUtR (Direct Connection Upgrade through Relay)** — used to
  upgrade an initial relay-mediated connection into a direct QUIC
  connection via UDP hole-punching. Requires a stable local UDP
  port on both sides — see `--udp-port` in
  [`../reference/config-flags.md`](../reference/config-flags.md).

Not implemented today: **AutoNAT**. Its role — probing whether the
local peer is publicly reachable — is currently played by the
relay/DCUtR interplay; adding AutoNAT would be an additional
optimization.

## Controlled networks

`--mdns` and `--outbound-allow` (SNIP #79) make a node's routes
predictable in a cooperative, controlled network: a lab, a staging
site, or a gateway whose clients must reach archives only through
configured endpoints. Both are unset by default, and unset they change
nothing.

> **Not a security boundary.** These are application-level routing
> controls. They make an honest `sum-node` keep to the endpoints its
> operator configured, and make every refused route observable. A
> modified or malicious client can ignore them, and they constrain no
> other process on the host. Where containment is the requirement, use
> a firewall or a sandbox.

### mDNS

`--mdns off` (`SUM_MDNS=off`; `false`, `0` and `no` are accepted too)
builds no mDNS behaviour at all: the node neither announces itself nor
discovers LAN peers by multicast. The default, `on`, is unchanged.
With mDNS off and `--enable-wan` unset the node has no way to discover
peers, and says so in a warning at startup.

### Outbound allow-list

`--outbound-allow <entry>` (repeatable or comma-separated;
`SUM_OUTBOUND_ALLOW=<entry>,<entry>`) restricts the addresses the node
connects over. Each entry is one of:

| Entry | Allows |
|---|---|
| `127.0.0.1`, `10.0.0.0/8`, `::1`, `2001:db8::/32` | An `/ip4` or `/ip6` host inside the network, on any transport and port, for any peer. A bare address is a single host. An IPv4-mapped IPv6 host (`::ffff:a.b.c.d`) also matches IPv4 networks. |
| `/ip4/127.0.0.1` | Every transport and port on that host. |
| `/ip4/127.0.0.1/tcp/4001` | TCP port 4001 on that host — not QUIC. |
| `/ip4/127.0.0.1/udp/4001/quic-v1` | QUIC on UDP port 4001 on that host — not TCP. `/udp/4001` alone allows the same. |
| `/dns4/relay.example.com/tcp/4001` | That DNS name (compared case-insensitively, same `/dns`, `/dns4`, `/dns6` or `/dnsaddr` kind) on TCP 4001. |
| any of the multiaddr forms, ending in `/p2p/<peer-id>` | The same, and only when the dial targets that peer; libp2p refuses the connection if the remote turns out to be another peer. |

A multiaddr entry matches an address that **begins with** the entry's
components. Further rules:

- **Relayed addresses** (`<relay address>/p2p-circuit/p2p/<peer>`) are
  judged by their relay part alone, because that is where the
  connection goes. The connection to the relay is a dial of its own
  and is judged again. A relay part with no host matches nothing.
- **DNS names are judged before resolution**, in the form the transport
  is asked to dial. A `/dns4/...` address never matches an IP network,
  and an allowed name is trusted to resolve wherever its records point.
- An address that does not start with a host (`/unix`, `/memory`,
  `/ip6zone`, ...) is never allowed.
- **Refused at startup** (usage error, exit 2): an entry containing
  `/p2p-circuit` (list the relay's own address instead), an entry that
  is only a peer ID (an allowed peer could advertise any address — the
  problem this flag exists for), port `0` (not a wildcard; leave the
  port out instead), and any other component after the host. Blank
  items, such as one left by a trailing comma, are ignored.

While an allow-list is set:

1. **Every outbound dial is judged** at the transport, after the swarm
   has merged every behaviour's addresses and before DNS resolution or
   any packet: bootstrap, mDNS, Identify, Kademlia, request-response,
   gossipsub, AutoNAT dial-back, DCUtR hole-punching (both roles) and
   relay-client dials to a relay, over TCP and QUIC alike. A refused
   address fails with `refused by the --outbound-allow policy`, and the
   dial moves on to its next address.
2. **Refused addresses are not stored.** Bootstrap entries, mDNS
   discoveries, Identify listen addresses and Kademlia routing updates
   are checked before they reach Kademlia, relay candidates, gossipsub
   or the upper layer, and peer addresses one behaviour publishes to
   the others are filtered too. An allowed peer advertising an
   alternative address therefore cannot get it dialed.
3. **Inbound connections are judged too.** A direct inbound connection
   is kept only when its remote address is allowed; a relayed one only
   when its relay is. Without this an allowed peer could open the
   second route itself by dialing in from an address that is not
   allowed (DCUtR, its own direct-dial shortcut and Kademlia all do).
   An archive that must accept direct client connections should leave
   `--outbound-allow` unset, or list its clients' hosts or networks
   (port-specific entries seldom match an inbound connection's source
   port).

Listeners are not changed by the policy. If every bootstrap peer is
refused, the Kademlia bootstrap query is skipped with a warning rather
than failing startup.

**Example.** A client that reaches two archives only through two TCP
endpoints (for example application-level relays in front of the
archives' TCP listeners):

```bash
sum-node --mdns off --enable-wan \
  --outbound-allow /ip4/127.0.0.1/tcp/4101,/ip4/127.0.0.1/tcp/4102 \
  --bootstrap-peer /ip4/127.0.0.1/tcp/4101/p2p/<archive-1-peer-id> \
  --bootstrap-peer /ip4/127.0.0.1/tcp/4102/p2p/<archive-2-peer-id> \
  download <merkle_root> --output out.bin
```

The same configuration for a `sum-node` child that OmniNode launches
through its environment:

```bash
SUM_MDNS=off
SUM_ENABLE_WAN=true
SUM_OUTBOUND_ALLOW=/ip4/127.0.0.1/tcp/4101,/ip4/127.0.0.1/tcp/4102
SUM_BOOTSTRAP_PEERS=/ip4/127.0.0.1/tcp/4101/p2p/<archive-1-peer-id>,/ip4/127.0.0.1/tcp/4102/p2p/<archive-2-peer-id>
```

The variables are read by every subcommand that networks (`listen`,
`ingest-v2`, `resume`, `download`, `fetch`, `send`). Closing an
endpoint stops the client's transfers through it; when it is back,
the next request reconnects through it.

### Observability

- **Startup**, once per networking command:
  `network policy: mdns=off outbound_allow=[/ip4/127.0.0.1/tcp/4101, /ip4/127.0.0.1/tcp/4102] (application-level routing control, not an OS security boundary)`;
  without a policy, `network policy: mdns=on outbound_allow=unrestricted`.
  Only the configured addresses appear; no key material or credentials.
- **Each refusal** is logged with its source, stage and address —
  at `warn` the first time a source offers an address, at `debug`
  after that:
  `network policy: address not in --outbound-allow — refused source="identify" stage="admission" addr=/ip4/127.0.0.1/udp/4001/quic-v1`.
  Sources: `bootstrap`, `mdns`, `identify`, `kad`, `autodial` (the
  Identify direct-dial shortcut, DCUtR, AutoNAT dial-back, relay-client
  dials), `inbound`, `other` (a dial no source claims). Stages:
  `admission` (refused before being stored), `dial` (refused as a dial
  target), `inbound` (a connection the remote opened).
- **Totals** by source, every 30 s when they changed and at shutdown:
  `network policy: refusal totals total=7 bootstrap=0 mdns=0 identify=4 kad=2 autodial=0 inbound=1 other=0`.
  They count refusals, not distinct addresses.
- **In process**, `SumNet::net_policy_refusals()` returns the same
  counts and `SumNet::connection_audit()` streams every connection
  established or closed (with its remote address) and every refusal.

These are log lines on the binary's usual log output. The `merkle_root:`
and `lifecycle:` lines and the exit statuses of `ingest-v2` and
`resume` are unchanged.

**Limits.** The policy is fixed for the life of the process. A refused
inbound connection has reached the node's socket before it is refused:
the kernel accepts the TCP connection or receives the QUIC handshake,
and the swarm drops it before any application protocol runs (an entry
pinned to a peer is checked after the libp2p handshake, when the peer
is known). Nothing here applies to the chain RPC connection
(`--rpc-url`), which is plain HTTP outside libp2p.

## Wire codecs

Two application protocols coexist on the same swarm:

- **`/sum/storage/v1`** — legacy V1 codec. Preserved so V1-registered
  files continue to serve and self-heal. No per-push Merkle proof.
- **`/sum/storage/v2`** — V2 codec. Per-push Merkle proof on the
  wire; typed `ShardRequestV2` / `ShardResponseV2` variants that
  encode Public / Private, `Pull` / `Push` / `ManifestPush` /
  `Ack`. Every V2 push is validated by
  `PushValidator::validate_push` in `sum-node` before persistence.

`VersionedShardCodec` in `crates/sum-net/src/codec.rs` dispatches
per-stream on the negotiated protocol name. There is no automatic
V2 → V1 fallback: a peer that doesn't advertise `/sum/storage/v2`
surfaces as an `OutboundFailure`, and the caller retries V1
explicitly if it wants to.

## Firewall requirements

For archives that expect to be dialed:

- **QUIC/UDP** on `--udp-port` — required.
- **TCP** on `--tcp-port` — required in WAN mode; some NAT
  environments accept only TCP.
- Egress to bootstrap peers (WAN) and to the chain RPC endpoint
  (always).

For archives behind a symmetric NAT: nothing needs to be opened.
Circuit Relay v2 + DCUtR handle the connectivity, provided the
relay server(s) are publicly reachable.

## Chunk lifecycle and garbage collection

Archives store chunks according to the current on-chain assignment
for their address. When the active-node set changes — an archive
joins, an archive leaves, an archive is slashed and flips out of
`Active` — the deterministic assignment algorithm recomputes and
some archives that used to hold a chunk may no longer be assigned
to it.

The `GarbageCollector` in `sum-store` reclaims that disk. It runs
automatically after each `MarketSync` cycle (default every 30
seconds) and:

1. Enumerates every `<cid>.chunk` file on the archive's disk.
2. Recomputes the current assignment from the on-chain snapshot.
3. Marks each chunk that this archive is no longer assigned to
   with a first-seen-unassigned timestamp.
4. Deletes chunks that have been unassigned for longer than
   `--gc-grace-secs` (default 1 hour).

Safety properties:

- **Never deletes a currently-assigned chunk.** GC always
  recomputes assignment first.
- **Grace period absorbs transient churn.** A node briefly leaving
  and rejoining does not trigger data loss — the chunk's marker is
  cleared as soon as the archive is re-assigned.
- **Paused on stale L1 state.** If the last successful RPC poll
  was more than 5 minutes ago, GC does not run — better to keep
  stale chunks than delete based on a stale assignment view.
- **Interaction with MarketSync.** When assignment recomputes to
  add a new archive, the new holder's `MarketSyncWorker` fetches
  the chunk within the 30 s cycle. That fetch completes well before
  the previous holder's 1 h grace period expires, so the network
  never drops below `R` copies during the transition.
- **No provenance tracking.** Chunks are stored identically as
  `<cid>.chunk`; GC does not distinguish "initially pushed to this
  archive by an ingest client" from "fetched via MarketSync." The
  only input to the keep-vs-delete decision is the current on-chain
  assignment.

For V2 files, retention is additionally enforced by chain-side
Proof of Retrievability challenges + slashing (see
[`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md));
GC is the disk-hygiene layer beneath that enforcement. V1 files
have no PoR-style guarantee — the `MarketSyncWorker` polling +
GC interaction is what keeps V1 files replicated.

## Cross-references

- Network policy implementation: `crates/sum-net/src/policy.rs` (rules, counters, audit) and `crates/sum-net/src/gate.rs` (enforcement).
- CLI flags that shape the networking surface: [`../reference/cli.md`](../reference/cli.md) global flags and
  [`../reference/config-flags.md`](../reference/config-flags.md).
- Mainnet bootstrap-peer plumbing: [`../operator/mainnet-bringup.md`](../operator/mainnet-bringup.md).
- Historical shipping narrative: [`../archive/WAN-DISCOVERY-AND-HARDENING.md`](../archive/WAN-DISCOVERY-AND-HARDENING.md).
