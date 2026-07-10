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

- **mDNS** (LAN, always on). Multicast DNS on the local link.
  Peers on the same LAN discover each other automatically without
  bootstrap. Suitable for local-mirror testing, LAN staging, and
  single-site deployments.
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

- CLI flags that shape the networking surface: [`../reference/cli.md`](../reference/cli.md) global flags and
  [`../reference/config-flags.md`](../reference/config-flags.md).
- Mainnet bootstrap-peer plumbing: [`../operator/mainnet-bringup.md`](../operator/mainnet-bringup.md).
- Historical shipping narrative: [`../archive/WAN-DISCOVERY-AND-HARDENING.md`](../archive/WAN-DISCOVERY-AND-HARDENING.md).
