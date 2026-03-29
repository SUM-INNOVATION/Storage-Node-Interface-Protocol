# Phase 4 Execution Plan: Scale-Out — Reed-Solomon, WAN Discovery & Production Hardening

## Overview

Phase 4 has four objectives:
1. **Resilient Upload — Multi-Node Push + Confirmation** — eliminate the single-point-of-failure window between upload and replication
2. **Reed-Solomon Erasure Coding** — replace 3x full replication with coded redundancy (3x -> ~1.5x overhead)
3. **WAN Discovery** — Kademlia DHT + NAT traversal so nodes work beyond LAN
4. **Production Hardening** — metrics, graceful shutdown, logging, full E2E test against live validators

---

## Objective 1: Resilient Upload — Multi-Node Push + Confirmation

**The problem:** Between Step 3 (Alice pushes chunks to N1) and Step 4 (other nodes fetch from N1), N1 is the sole holder of the file. If N1 discards or corrupts the data during this window, the file is permanently lost. No amount of PoR challenges or slashing can recover data that never existed on a second node.

**The solution:** Alice pushes to R=3 nodes directly (not just N1), and retains her local copy until she has confirmed all 3 nodes hold the data via gossipsub ChunkAnnouncements.

**Current flow (vulnerable):**
```
Alice -> N1 (single copy)
   ⚠️ WINDOW: N1 is sole holder, file can be lost
N1 announces via gossipsub
Other nodes eventually fetch from N1
```

**Proposed flow (resilient):**
```
Alice computes assignment: chunks 0-9 assigned to [N1,N3,N7], [N5,N2,N9], etc.
Alice pushes each chunk directly to its R=3 assigned nodes in parallel
Alice waits for R ChunkAnnouncements per chunk (confirmation)
Alice disconnects only after all C*R confirmations received (or timeout)
```

**Files to create/modify:**

| File | Action | What |
|------|--------|------|
| `sum-node/src/upload.rs` | **New** | `UploadOrchestrator` — takes a DataManifest, computes the assignment (requires L1 RPC to get active nodes), pushes each chunk to its R assigned nodes in parallel. Tracks confirmations via gossipsub. Returns `UploadResult { confirmed: u32, total: u32, timeout: bool }` |
| `sum-store/src/lib.rs` | **Modify** | `SumStore::ingest_file()` returns the assignment alongside the manifest, so the upload orchestrator knows where to push |
| `sum-node/src/main.rs` | **Modify** | The `ingest` subcommand uses `UploadOrchestrator` instead of pushing to the first discovered peer. New `--upload-timeout` flag (default 120s). Exits with error if fewer than R confirmations received per chunk |
| `sum-net/src/lib.rs` | **Modify** | Add `push_chunk(peer_id, cid, data)` method — inverse of `request_shard_chunk`. Sends chunk data to a specific peer proactively |

**Design details:**
- Alice must call `storage_getActiveNodes()` via RPC to compute the assignment before uploading. This means Alice needs `--rpc-url` to know which nodes are assigned.
- If an assigned node is unreachable, Alice falls back to pushing to the next node in the sorted list (same linear probing as the assignment algorithm).
- Confirmation is via gossipsub: Alice subscribes to `sum/storage/v1` and waits for `ChunkAnnouncement` messages matching her merkle_root from the expected PeerIds.
- Timeout behavior: if after `--upload-timeout` seconds fewer than R confirmations are received for any chunk, Alice logs a warning with the specific chunks/nodes that didn't confirm. She does NOT disconnect silently — the user sees exactly what failed.

**Tests (target: 8+ new tests):**
- Upload to 3 nodes in parallel, all confirm -> success
- Upload with 1 unreachable node -> falls back to next node, still R confirmations
- Upload timeout with 0 confirmations -> returns error with details
- Upload with 2 of 3 confirmations + timeout -> returns partial success warning
- Confirmation matching: announcements from wrong PeerId or wrong merkle_root are ignored
- Assignment computation matches what nodes would independently compute
- Push + announce round-trip: Alice pushes chunk, node announces it, Alice sees the announcement
- Integration: full ingest with 3 nodes, verify all 3 hold the chunks on disk

---

## Objective 2: Reed-Solomon Erasure Coding

**What changes:** Instead of storing 3 identical copies of each chunk, we encode each chunk into `k` data shards + `m` parity shards using Reed-Solomon. Any `k` of the `k+m` shards can reconstruct the original chunk.

**Parameters:** `k=4, m=2` (4 data + 2 parity = 6 shards per chunk, tolerate loss of any 2). Storage overhead drops from 3x to 1.5x.

**Files to create/modify:**

| File | Action | What |
|------|--------|------|
| `sum-store/src/erasure.rs` | **New** | `ReedSolomonEncoder` wrapping the `reed-solomon-erasure` crate. `encode_chunk(data) -> Vec<Shard>`, `decode_chunk(shards) -> Vec<u8>` |
| `sum-types/src/storage.rs` | **Modify** | Add `ERASURE_DATA_SHARDS = 4`, `ERASURE_PARITY_SHARDS = 2`, `ERASURE_TOTAL_SHARDS = 6`. Add `ShardDescriptor` type (shard_index, parent_chunk_index, blake3_hash, cid, size, is_parity) |
| `sum-store/src/chunker.rs` | **Modify** | After chunking into 1 MB pieces, pass each chunk through the RS encoder. The `DataManifest` now contains `Vec<ShardDescriptor>` grouped by parent chunk. Merkle leaves become shard hashes (not chunk hashes) |
| `sum-store/src/merkle.rs` | **No change** | Still builds a tree from leaf hashes — the leaves are now shard hashes instead of chunk hashes. The algorithm is identical |
| `sum-store/src/assignment.rs` | **Modify** | Assignment granularity changes from chunks to shards. Each shard is assigned to 1 node (not 3 replicas). The RS coding provides redundancy instead of replication. `compute_chunk_assignment()` -> `compute_shard_assignment()` |
| `sum-store/src/store.rs` | **Minor** | Files stored as `<cid>.shard` instead of `<cid>.chunk` (cosmetic) |
| `sum-store/src/serve.rs` | **Minor** | Serve shards instead of chunks |
| `sum-node/src/por_worker.rs` | **Modify** | PoR proof now provides shard hash + merkle path for the shard leaf |
| `sum-node/src/market_sync.rs` | **Modify** | Fetches assigned shards (not chunk replicas) |
| Cargo.toml | **Add dep** | `reed-solomon-erasure = "6.0"` |

**L1 changes required (sum-chain `SNIP-Compatibility-Modifications`):**
- `REPLICATION_FACTOR` becomes less relevant — replaced by `ERASURE_TOTAL_SHARDS`
- `generate_challenge()` must challenge on shard indices, not chunk indices
- `verify_storage_proof()` must verify against shard-level merkle leaves
- `compute_chunk_assignment()` -> `compute_shard_assignment()` with same hash formula but different count

**Tests (target: 15+ new tests):**
- RS encode/decode round-trip (exact reconstruction)
- RS decode with 1 missing shard, 2 missing shards
- RS decode fails with 3+ missing shards
- Shard hashes are deterministic
- Merkle tree built from shard hashes matches manual computation
- PoR proof for shard index verifies against L1 algorithm
- Assignment distributes shards evenly (6 shards x N chunks across M nodes)
- DataManifest with shards serializes/deserializes (CBOR + JSON)
- End-to-end: ingest file -> encode -> build tree -> generate proof -> verify

---

## Objective 3: WAN Discovery (Kademlia DHT + NAT Traversal)

**What changes:** Currently nodes only find each other via mDNS (same LAN). We add Kademlia DHT for internet-wide peer discovery, AutoNAT for detecting NAT type, and DCUtR (Direct Connection Upgrade through Relay) for hole-punching behind NATs.

**Files to create/modify:**

| File | Action | What |
|------|--------|------|
| `sum-net/src/transport.rs` | **Implement** | TCP/Noise transport alongside existing QUIC. Some NATs block QUIC (UDP); TCP fallback ensures connectivity. `build_transport(keypair) -> Boxed<(PeerId, StreamMuxerBox)>` |
| `sum-net/src/nat.rs` | **Implement** | AutoNAT behaviour config + DCUtR relay client. Detects if node is behind NAT, uses relay for initial connection, then upgrades to direct via hole-punching |
| `sum-net/src/capability.rs` | **Implement** | Gossipsub-based capability advertisement. Nodes announce: PeerId, L1 address, listen addresses, available disk, shard count. Other nodes use this to find who holds what |
| `sum-net/src/behaviour.rs` | **Modify** | Add `kademlia`, `autonat`, `relay_client`, `dcutr` to `LocalMeshBehaviour` |
| `sum-net/src/swarm.rs` | **Modify** | Handle Kademlia events (RoutingUpdated, QueryResult), AutoNAT events (StatusChanged), DCUtR events. Add `SwarmCommand::Bootstrap` for initial DHT population |
| `sum-net/src/discovery.rs` | **Modify** | Add `handle_kademlia_event()` alongside existing `handle_mdns_event()`. Kademlia provides WAN peers, mDNS provides LAN peers — both feed into the same event stream |
| `sum-net/src/lib.rs` | **Modify** | Add `bootstrap()` method to trigger Kademlia bootstrap. Add config for bootstrap peers |
| `sum-types/src/config.rs` | **Modify** | Add `bootstrap_peers: Vec<String>` (multiaddrs), `enable_wan: bool`, `relay_addrs: Vec<String>` to `NetConfig` |
| `sum-node/src/main.rs` | **Modify** | Add `--bootstrap-peer` (repeatable), `--enable-wan`, `--relay` CLI flags. Call `bootstrap()` on startup when WAN is enabled |

**Bootstrap peer format:** `/ip4/1.2.3.4/tcp/4001/p2p/12D3KooW...` — at least 2-3 well-known SUM network bootstrap nodes needed for production.

**Tests (target: 10+ new tests):**
- TCP/Noise transport handshake completes (loopback)
- Kademlia `add_address` + `bootstrap` doesn't panic with empty peer list
- Capability announcement encode/decode round-trip
- Capability announcement contains correct L1 address and PeerId
- AutoNAT config builds without error
- DCUtR relay client config builds without error
- NetConfig with bootstrap peers serializes/deserializes
- Discovery module emits PeerDiscovered from both mDNS and Kademlia sources
- Two nodes on localhost discover each other via Kademlia (no mDNS)

---

## Objective 4: Production Hardening

**Files to create/modify:**

| File | Action | What |
|------|--------|------|
| `sum-node/src/metrics.rs` | **New** | Prometheus-compatible metrics: chunks_stored, chunks_served, por_challenges_answered, por_challenges_missed, peers_connected, bytes_transferred, assignment_coverage_percent |
| `sum-node/src/main.rs` | **Modify** | Graceful shutdown: on SIGINT/SIGTERM, stop accepting new requests, flush pending PoR proofs, close swarm, write final metrics |
| `sum-store/src/lib.rs` | **Modify** | `SumStore::health_check()` — verifies store dir writable, manifest index loadable, reports chunk count and disk usage |
| `tests/integration/test_e2e_l1.sh` | **Modify** | Run against live validators. Validate full loop: register node -> ingest file -> register on L1 -> wait for challenge -> verify proof accepted -> check Koppa earned |
| `tests/integration/test_wan_discovery.sh` | **New** | Spawn 3 nodes: 1 bootstrap, 2 clients on different ports. Verify Kademlia discovery works without mDNS |
| `tests/integration/test_erasure.sh` | **New** | Ingest file with RS encoding, kill 2 of 6 shard holders, verify file is still reconstructable from remaining 4 |

**Tests (target: 8+ new tests):**
- Metrics counter increments correctly
- Health check passes on valid store, fails on missing dir
- Graceful shutdown completes within timeout
- E2E L1 test (full PoR loop with live validators)

---

## Execution Order

```
Step 1: Objective 1 (Resilient Upload) — HIGHEST PRIORITY
  |-- 1a. sum-net: push_chunk() method (proactive send)
  |-- 1b. sum-node/upload.rs: UploadOrchestrator
  |-- 1c. sum-store: ingest returns assignment alongside manifest
  |-- 1d. sum-node/main.rs: ingest uses UploadOrchestrator
  +-- 1e. Tests: 8+ unit tests + multi-node integration test

Step 2: Objective 2 (Reed-Solomon)
  |-- 2a. sum-types: Add erasure constants + ShardDescriptor
  |-- 2b. sum-store/erasure.rs: RS encoder/decoder
  |-- 2c. sum-store/chunker.rs: Integrate RS into chunking pipeline
  |-- 2d. sum-store/assignment.rs: Shard-level assignment
  |-- 2e. sum-node/por_worker.rs + market_sync.rs: Shard-aware
  |-- 2f. L1 changes (sum-chain): Shard-aware challenges + verification
  +-- 2g. Tests: 15+ unit tests for erasure + assignment + PoR

Step 3: Objective 3 (WAN Discovery)
  |-- 3a. sum-net/transport.rs: TCP/Noise fallback
  |-- 3b. sum-net/nat.rs: AutoNAT + DCUtR
  |-- 3c. sum-net/capability.rs: Gossipsub capability ads
  |-- 3d. sum-net/behaviour.rs + swarm.rs: Wire in Kademlia + NAT
  |-- 3e. sum-net/discovery.rs: Kademlia event handling
  |-- 3f. sum-node/main.rs: CLI flags + bootstrap
  +-- 3g. Tests: 10+ unit tests + WAN integration script

Step 4: Objective 4 (Production Hardening)
  |-- 4a. sum-node/metrics.rs: Prometheus metrics
  |-- 4b. Graceful shutdown + health checks
  |-- 4c. E2E L1 integration test against live validators
  +-- 4d. Tests: 8+ unit tests + integration scripts
```

---

## Total New Tests: ~41+
## Total Expected Tests After Phase 4: ~118+
