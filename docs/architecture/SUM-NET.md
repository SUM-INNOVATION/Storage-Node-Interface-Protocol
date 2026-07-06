# sum-net

The networking crate. It wraps libp2p into the peer-to-peer mesh that moves
chunks and manifests between nodes, handles LAN and WAN discovery, traverses NAT,
and derives node identity. It depends only on `sum-types`. Source:
[`crates/sum-net`](../../crates/sum-net).

## The behaviour stack

The composed `NetworkBehaviour` (`LocalMeshBehaviour` in
[`behaviour.rs`](../../crates/sum-net/src/behaviour.rs)) bundles nine libp2p
protocols:

| Protocol | Role |
|----------|------|
| `mdns` | LAN peer discovery via link-local multicast |
| `gossipsub` | pub/sub messaging (chunk announcements, capability, test) |
| `identify` | protocol negotiation, address exchange, relay-capability detection |
| `request_response` (shard_xfer) | the chunk and manifest transfer protocol, V1 and V2 |
| `kademlia` | DHT for WAN peer discovery |
| `autonat` | NAT reachability probing (public vs private) |
| `relay` | Circuit Relay v2 server (accept reservations from firewalled peers) |
| `relay_client` | Circuit Relay v2 client (request reservations) |
| `dcutr` | Direct Connection Upgrade through Relay (hole-punch a relayed circuit into a direct one) |

## Transports

Built in [`swarm.rs`](../../crates/sum-net/src/swarm.rs):

- **QUIC** (`quic-v1` over UDP) is always on, listening on
  `/ip4/0.0.0.0/udp/<udp_port>/quic-v1`. It is the primary transport.
- **TCP** (with Noise encryption and Yamux multiplexing) is added only in WAN
  mode, listening on `/ip4/0.0.0.0/tcp/<tcp_port>`.
- **DNS** resolution wraps the stack, and a **relay-client** transport is layered
  in for NAT bypass.

Idle connections time out after 60 seconds. Ports default to `0` (OS-assigned);
pin `--udp-port` when a node must be reliably dialable over QUIC (for a fixed
DCUtR hole-punch target).

## Discovery

Two mechanisms, chosen by the `--enable-wan` flag
([`discovery.rs`](../../crates/sum-net/src/discovery.rs)):

- **mDNS (LAN):** always active. Discovered peers are automatically added to
  gossipsub as explicit peers.
- **Kademlia DHT (WAN):** active with `--enable-wan`. Protocol id `/sum/kad/1.0.0`,
  server mode (every node is a full DHT participant), 60-second query timeout,
  1-hour record TTL, republish every 10 minutes. Bootstrap peers are supplied as
  multiaddrs ending in `/p2p/<peer_id>`; each is added to the routing table,
  stashed as an unconfirmed relay candidate, and dialed.

## NAT traversal

The interesting part, in [`nat.rs`](../../crates/sum-net/src/nat.rs). Three
protocols cooperate to get a node behind residential NAT onto the mesh.

**AutoNAT** probes reachability and resolves to one of three states: `Unknown`
(awaiting first probe), `Public(addr)` (externally dialable), or `Private`
(behind NAT, needs a relay). It retries every 60 seconds and requires 3
confirmations before declaring a verdict, refreshing every 300 seconds
thereafter.

**Circuit Relay v2** gives a `Private` node a reachable address. When a node runs
as a relay server (`--relay-server` on a public host), it accepts reservations
under tight limits: 128 reservations globally, 4 per peer, 120-second circuit
duration, 8 MiB per circuit, plus per-peer and per-IP rate limits. Circuits are
deliberately short-lived and small, because they are only a stepping stone to a
direct connection.

The node's own reservation is tracked by a three-state machine to prevent
stacking: `None → Pending(peer) → Active(peer)`, with any failure (denial,
listener close, timeout) calling `reset()` so the state cannot wedge. Only relay
candidates confirmed by `identify` to actually speak the relay hop protocol are
eligible; bootstrap peers start unconfirmed and are promoted.

**DCUtR** then upgrades a relayed circuit into a direct connection by
hole-punching, emitting `HolePunchSucceeded` or `HolePunchFailed`. The end state
is a direct QUIC connection with the relay dropped.

## The wire codec

[`codec.rs`](../../crates/sum-net/src/codec.rs) defines the chunk-transfer
protocol. The frame format is `[u32 big-endian length][bincode payload]`, with a
256 MiB message cap. Bincode is used (not JSON) because responses carry raw
`Vec<u8>` chunk bytes, where base64's 33% tax would be significant.

Two protocol versions coexist, negotiated at the libp2p layer:

- **`/sum/storage/v1`** (`ShardRequest` / `ShardResponse`): the legacy
  request/response, where a push is signalled by an optional `push_data` field.
- **`/sum/storage/v2`** (`ShardRequestV2` / `ShardResponseV2`): explicit variants.
  Requests are `Pull { cid, offset, max_bytes }`,
  `Push { data, merkle_root, chunk_index, merkle_path }`,
  `ManifestPush { merkle_root, manifest_bytes }`, and
  `ManifestPull { merkle_root }`. Responses are `Data`, `PushAck`,
  `ManifestPushAck`, and `ManifestData`, each carrying an `error: Option<String>`.

The V2 `Push` carries an inline `merkle_path` so the receiver can verify the
chunk against the on-chain root before writing it (the check lives in
`sum-store`; see [`SUM-STORE.md`](SUM-STORE.md)). A single `VersionedShardCodec`
dispatches on the negotiated protocol name, V2 is offered first when both peers
support both, and writing a V2 variant to a V1 stream (or vice versa) is rejected
at write time rather than silently mis-encoded.

## Gossipsub topics

Three topics, all `v1`, all with strict signature validation and a 10-second
heartbeat:

- `sum/storage/v1`: chunk availability announcements (`ChunkAnnouncement`, encoded
  in `sum-store`).
- `sum/capability/v1`: node capability advertisement.
- `sum/test/v1`: the `send` diagnostic command's channel.

## Identity

[`identity.rs`](../../crates/sum-net/src/identity.rs) derives both of a node's
identities from the same Ed25519 keypair:

- **PeerId**: `keypair.public().to_peer_id()`, the standard libp2p multihash.
- **L1 address**: `blake3(ed25519_pubkey_bytes)[12..32]`, the last 20 bytes of the
  BLAKE3 hash of the public key. This matches the chain's address derivation
  exactly, so the chain can always map a peer to its L1 account. The base58
  encoding appends a 4-byte double-BLAKE3 checksum.

When a peer connects, the `identify` exchange yields its public key, from which
the node derives that peer's L1 address (`l1_address_from_peer_public_key`) and
emits `PeerIdentified { peer_id, l1_address }`. This is what lets the ACL gate map
an inbound request to an on-chain account.

## See also

- [`SUM-STORE.md`](SUM-STORE.md): the push validator and Merkle verification the V2 codec feeds
- [`V1-VS-V2.md`](V1-VS-V2.md): why two protocol versions coexist
- [`OVERVIEW.md`](OVERVIEW.md): where this crate sits in the workspace
