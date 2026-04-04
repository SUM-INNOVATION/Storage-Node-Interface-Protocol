# WAN Discovery + Production Hardening Implementation

## Overview

This document describes the WAN discovery and production hardening changes implemented for the SUM Storage Node Protocol. These changes enable peer discovery beyond LAN (via Kademlia DHT + TCP/Noise transport) and add production-grade shutdown, metrics, health checks, and error handling.

**Prior state:** Nodes could only discover each other via mDNS (same LAN). Background workers ran in infinite loops with no shutdown signal, no backoff on failure, and unbounded memory usage. No metrics or health checks existed.

**After these changes:** Nodes can discover each other across the internet via Kademlia DHT with TCP fallback. Background workers respond to graceful shutdown signals, back off exponentially on RPC failure, and use bounded data structures.

---

## Phase 1: WAN Discovery

### TCP/Noise/Yamux Transport

When `--enable-wan` is set, the node listens on both QUIC (UDP) and TCP (with Noise encryption + Yamux stream multiplexing). This is important because some NATs and firewalls block UDP, which would prevent QUIC-only nodes from connecting. TCP provides a reliable fallback.

**How it works:**
- `SumSwarm::build()` checks `config.enable_wan`
- If true: builds with `.with_tcp(tcp::Config, noise::Config, yamux::Config).with_quic()` — dual transport
- If false: builds with `.with_quic()` only — existing LAN behavior, no change
- TCP listener binds to `/ip4/0.0.0.0/tcp/{tcp_listen_port}`
- QUIC listener always binds to `/ip4/0.0.0.0/udp/{listen_port}/quic-v1`

**Files changed:**
- `Cargo.toml` — added `yamux` to libp2p features
- `crates/sum-net/src/swarm.rs` — conditional dual transport in `build()`

### Kademlia DHT

Kademlia is a distributed hash table protocol that enables peer discovery without a central server. Each node maintains a routing table of known peers. When a node joins, it contacts bootstrap peers who introduce it to other nodes in the network. Peer addresses propagate through the DHT until all nodes can find each other.

**How it works:**
1. `LocalMeshBehaviour` now includes `kademlia: kad::Behaviour<kad::store::MemoryStore>`
2. On startup (when `enable_wan` is true), `bootstrap_kademlia()` is called:
   - Parses each `--bootstrap-peer` multiaddr (e.g., `/ip4/1.2.3.4/tcp/4001/p2p/12D3KooW...`)
   - Extracts the PeerId from the `/p2p/` component
   - Adds the address to Kademlia's routing table
   - Dials the peer
   - Initiates a Kademlia bootstrap query to populate the routing table
3. When the Identify protocol reveals a peer's listen addresses, those addresses are fed into Kademlia via `kademlia.add_address()` — this is how the DHT grows beyond bootstrap nodes
4. When Kademlia discovers a new peer (`RoutingUpdated` event):
   - The peer is added to gossipsub via `add_explicit_peer()` (same as mDNS does for LAN peers)
   - A `PeerDiscovered` event is emitted to the application layer
5. Both mDNS (LAN) and Kademlia (WAN) run simultaneously — LAN peers are discovered instantly via mDNS, WAN peers via DHT

**Kademlia protocol ID:** `/sum/kad/1.0.0`

**Files changed:**
- `crates/sum-net/src/behaviour.rs` — added `kademlia` field
- `crates/sum-net/src/swarm.rs` — Kademlia construction, `bootstrap_kademlia()`, event handling, Identify→Kademlia feed
- `crates/sum-net/src/lib.rs` — calls `bootstrap_kademlia()` in `SumNet::new()` when WAN enabled

### CLI Flags

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--enable-wan` | `SUM_ENABLE_WAN` | false | Enable Kademlia DHT + TCP transport |
| `--bootstrap-peer` | `SUM_BOOTSTRAP_PEERS` | empty | Bootstrap peer multiaddrs (repeatable or comma-separated) |
| `--tcp-port` | `SUM_TCP_PORT` | 0 (OS-assigned) | TCP listen port for WAN connections |

**All 5 command flows** (listen, ingest, fetch, download, send) now receive and use the `NetConfig` built from these CLI args. `NetConfig::default()` is no longer used anywhere in main.rs.

### NetConfig

```rust
pub struct NetConfig {
    pub listen_port: u16,             // QUIC/UDP port (0 = OS-assigned)
    pub tcp_listen_port: u16,         // TCP port (0 = OS-assigned)
    pub enable_wan: bool,             // false = mDNS-only
    pub bootstrap_peers: Vec<String>, // multiaddr strings for Kademlia bootstrap
}
```

---

## Phase 2: Production Hardening

### Graceful Shutdown

**Problem:** PorWorker and MarketSyncWorker were spawned as fire-and-forget `tokio::spawn` tasks with infinite loops. On Ctrl-C, the swarm shut down but workers kept running (or were killed mid-operation).

**Solution:** A `tokio::sync::watch::channel(false)` is created in `run_listen()`. Each worker receives a `watch::Receiver<bool>`. Workers check for shutdown in their `tokio::select!` loop:

```rust
tokio::select! {
    _ = interval.tick() => { /* normal work */ }
    _ = shutdown.changed() => {
        if *shutdown.borrow() {
            info!("worker shutting down");
            // One final flush
            return;
        }
    }
}
```

On Ctrl-C, the main loop:
1. Sends `true` on the shutdown channel
2. Waits 5 seconds for workers to flush pending work
3. Shuts down the swarm

**Files changed:**
- `crates/sum-node/src/main.rs` — creates channel, passes to workers, 5s flush on Ctrl-C
- `crates/sum-node/src/por_worker.rs` — accepts `watch::Receiver<bool>`, checks in select!
- `crates/sum-node/src/market_sync.rs` — same

### Health Check

`SumStore::health_check()` returns a `HealthReport`:

```rust
pub struct HealthReport {
    pub chunk_count: usize,       // number of .chunk files on disk
    pub manifest_count: usize,    // number of indexed manifests
    pub disk_usage_bytes: u64,    // total bytes used by chunk files
    pub store_dir_writable: bool, // can we write to the store directory?
}
```

The writable check writes a probe file and removes it. If the write fails, the store is marked as not writable.

**Files changed:**
- `crates/sum-store/src/lib.rs` — `health_check()` method + `HealthReport` struct

### Metrics

`NodeMetrics` provides atomic counters for key operational metrics:

```rust
pub struct NodeMetrics {
    pub chunks_served: AtomicU64,
    pub por_proofs_submitted: AtomicU64,
    pub por_proofs_failed: AtomicU64,
    pub gc_chunks_deleted: AtomicU64,
    pub peers_connected: AtomicU64,
}
```

No external crate needed — uses `std::sync::atomic`. Thread `Arc<NodeMetrics>` through the system. Provides `snapshot()` for point-in-time reads.

**Files created:**
- `crates/sum-node/src/metrics.rs`

### Exponential Backoff

**Problem:** When the L1 RPC is unreachable, workers retried immediately on every poll tick, hammering the endpoint.

**Solution:** Both PorWorker and MarketSyncWorker track `consecutive_failures: u32`. On failure:
```
backoff = poll_interval × 2^min(failures, 5)
```

For a 10-second poll interval: first failure waits 20s, second 40s, third 80s, capping at 320s (5 minutes). On success, failures reset to 0.

**Files changed:**
- `crates/sum-node/src/por_worker.rs`
- `crates/sum-node/src/market_sync.rs`

### Bounded Data Structures

**PorWorker `responded` set:** Changed from `HashSet<String>` (unbounded, cleared at 1000) to `Vec<String>` with FIFO eviction at 500 entries. When full, the oldest entry is removed before inserting new ones. O(n) lookup on 500 entries is negligible.

**MarketSyncWorker `pending_fetches`:** Still a `HashSet<String>`, but now cleared every 5 minutes via `last_fetches_cleared` timestamp. Stale entries from failed fetches no longer accumulate indefinitely.

**Files changed:**
- `crates/sum-node/src/por_worker.rs` — `responded: Vec<String>`, `MAX_RESPONDED = 500`
- `crates/sum-node/src/market_sync.rs` — `last_fetches_cleared: Instant`, clear every 300s

---

## Test Results

**96 tests passing** (90 prior + 6 new). New tests:

- `health_check_empty_store` — empty store: 0 chunks, 0 manifests, writable
- `health_check_with_chunks` — 2 chunks on disk: count=2, disk_usage > 0, writable
- `metrics_increment_and_snapshot` — increment all counters, verify snapshot values match
- `metrics_peers_inc_dec` — inc 3 peers, dec 1, verify peers_connected=2
- `metrics_default_is_zero` — fresh NodeMetrics, all counters zero
- `metrics_gc_batch_increment` — inc_gc_deleted(10) + inc_gc_deleted(25) = 35

---

## How to Use WAN Mode

### Two nodes on different networks

**Node A** (public IP 1.2.3.4):
```bash
sum-node --key-file node_a.key --rpc-url http://<validator>:9944 \
  --enable-wan --tcp-port 4001 listen
```
Note Node A's PeerId from the startup log: `local_peer_id=12D3KooW...`

**Node B** (different network):
```bash
sum-node --key-file node_b.key --rpc-url http://<validator>:9944 \
  --enable-wan --tcp-port 4001 \
  --bootstrap-peer /ip4/1.2.3.4/tcp/4001/p2p/12D3KooW... \
  listen
```

Node B bootstraps off Node A. Kademlia propagates the routing table. Both nodes discover each other and join the gossipsub mesh.

### Alice uploading from a different network

```bash
sum-node --client --rpc-url http://<validator>:9944 \
  --enable-wan \
  --bootstrap-peer /ip4/1.2.3.4/tcp/4001/p2p/12D3KooW... \
  ingest ./textbook.pdf
```

### LAN mode (unchanged default)

```bash
sum-node listen
```

No `--enable-wan` → mDNS only → same behavior as before. Zero breaking changes.
