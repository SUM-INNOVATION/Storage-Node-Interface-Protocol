# Client Mode Gap: Alice Should Not Be a Storage Node

## Context

The README describes two types of actors:

- **Storage nodes (N1-N10):** Long-running daemons that stake Koppa, register as ArchiveNodes, hold chunks on disk, respond to PoR challenges, earn rewards. They are always online.
- **External clients (Alice, Bob):** Transient users with wallets. Alice uploads a file and disconnects. Bob downloads a file and disconnects. Neither stakes, answers challenges, or serves chunks. They are users of the infrastructure, not the infrastructure itself.

This distinction is central to the protocol's design — anyone with a SUM Chain wallet and Koppa should be able to store and retrieve files without running storage infrastructure.

**The current implementation does not support this.** Alice must run a full storage node to upload a file.

---

## What the Code Currently Does

### `run_ingest()` in `crates/sum-node/src/main.rs`

When Alice runs `sum-node ingest file.pdf`, the following happens:

1. **A full libp2p swarm starts** — `SumNet::new()` creates a QUIC transport, subscribes to all gossipsub topics (`sum/storage/v1`, `sum/test/v1`, `sum/capability/v1`), starts mDNS discovery, and begins listening on a port. Alice is now a reachable peer on the mesh.

2. **The file is chunked and stored locally on Alice's disk** — `store.ingest_file(&path)` writes all chunks to `~/.sumnode/store/<cid>.chunk`. Alice is now holding the data on her own machine.

3. **Alice waits for a peer** — she sits in a loop until mDNS discovers another node on the LAN.

4. **Alice announces chunks via gossipsub** — `store.announce_chunks()` publishes a `ChunkAnnouncement` for each chunk, telling the network "I have these chunks."

5. **Alice enters `simple_serve_loop()` and stays running indefinitely** — she responds to incoming `ShardRequested` events, serving chunks to any peer that asks. She is functionally a storage node at this point.

6. **Alice never exits** — the only way to stop is Ctrl-C. She serves chunks until manually killed.

### What's wrong with this

| What the README says | What the code does |
|---------------------|-------------------|
| Alice is a client, not a node | Alice runs a full node with swarm, gossipsub, and chunk serving |
| Alice pushes to R=3 assigned nodes | Alice pushes to 0 nodes — she announces and waits for others to pull |
| Alice disconnects after R=3 confirmations | Alice never disconnects — she serves forever |
| Alice doesn't need to register or stake | Correct — no registration needed to ingest |
| Alice doesn't store data long-term | Alice stores all chunks on her own disk indefinitely |

### The UploadOrchestrator exists but isn't wired in

`crates/sum-node/src/upload.rs` contains an `UploadOrchestrator` that does the right thing:
- Computes the assignment from L1 state
- Pushes each chunk to R=3 assigned nodes via `net.push_chunk()`
- Tracks ACK confirmations
- Returns an `UploadResult` with confirmed/failed counts

But `run_ingest()` does NOT use it. It still uses the old single-peer announce-and-serve flow.

### The download command is closer to correct

`crates/sum-node/src/download.rs` is better — Bob fetches chunks, assembles the file, and the orchestrator returns a result. However, `run_download()` in main.rs still creates a full `SumNet` with all gossipsub subscriptions. A true lightweight client wouldn't need gossipsub subscriptions or to be a reachable peer.

---

## What Needs to Change

### Phase 1: Wire UploadOrchestrator into `run_ingest()`

**File:** `crates/sum-node/src/main.rs`

Replace the current `run_ingest()` flow:

```
CURRENT:
  1. Start full swarm
  2. Ingest file to local disk
  3. Wait for peer discovery
  4. Announce via gossipsub
  5. Enter simple_serve_loop() (serves forever)

PROPOSED:
  1. Start swarm (still needed for mDNS + push protocol)
  2. Ingest file to local disk (temporary — for reading chunks to push)
  3. Wait for peer discovery
  4. Query L1 for active nodes, compute assignment
  5. Push each chunk to R=3 assigned nodes via UploadOrchestrator
  6. Wait for R confirmations per chunk (or timeout)
  7. Report result
  8. Exit (do NOT enter serve loop)
```

**Key changes:**
- Remove `simple_serve_loop()` from the ingest flow
- Add `UploadOrchestrator::run()` after ingestion
- Add `--upload-timeout` flag (default 120s)
- After successful upload, optionally clean up local chunks (Alice doesn't need them anymore)
- Exit with code 0 on full confirmation, code 1 on partial/timeout

### Phase 2: Lightweight Client Mode

**Goal:** A `--client` flag or separate binary that minimizes the libp2p footprint.

**What a client needs:**
- QUIC transport (for push/fetch protocol)
- mDNS or direct peer address (for finding nodes)
- `/sum/storage/v1` request-response (for push and fetch)
- Gossipsub subscription to `sum/storage/v1` (for tracking ChunkAnnouncements as confirmations)

**What a client does NOT need:**
- PorWorker (no challenges to answer — not registered)
- MarketSyncWorker (no assigned chunks to fetch)
- GarbageCollector (no long-term storage)
- ACL checker (no chunks to serve)
- Chunk serving (handle_request / simple_serve_loop)
- Persistent chunk store (only temporary during upload)

**Implementation options:**

**Option A — `--client` flag on `sum-node`:**
```bash
sum-node --client ingest file.pdf --rpc-url http://validator:9944
sum-node --client download <merkle_root> --output ./file.pdf
```
When `--client` is set:
- `run_ingest()` uses UploadOrchestrator, exits after confirmation
- `run_download()` already exits after assembly (no change needed)
- No `listen` command allowed
- Store directory is a temp dir, cleaned up on exit

**Option B — Separate `sum-client` binary:**
A distinct binary with only `upload` and `download` subcommands. No `listen` command. Uses a temporary store directory. Simpler CLI, no confusion with node commands.

**Recommendation:** Option A first (less code duplication), Option B later for production.

### Phase 3: Minimize swarm for client mode

Even with `--client`, the current `SumNet::new()` starts a full swarm with all behaviours. For true lightweight mode:
- Skip gossipsub subscriptions (use ShardReceived ACKs for confirmation instead of ChunkAnnouncements)
- Or: subscribe to `sum/storage/v1` temporarily, unsubscribe on exit
- Don't start listening on a port (client is outbound-only, not a reachable peer)

This is a deeper change to `sum-net` and can come later. Phase 1 (wiring UploadOrchestrator) gives 90% of the benefit with minimal code changes.

---

## Files to Modify

| File | Change |
|------|--------|
| `crates/sum-node/src/main.rs` | Replace `simple_serve_loop()` in `run_ingest()` with `UploadOrchestrator::run()`. Add `--upload-timeout` flag. Exit after confirmation. |
| `crates/sum-node/src/main.rs` | Add `--client` flag to Cli struct. When set, skip PorWorker/MarketSync/GC in listen mode (or disallow listen entirely). |
| `crates/sum-node/src/upload.rs` | No changes — already implemented correctly. |
| `crates/sum-node/src/download.rs` | No changes — already exits after assembly. |
| `crates/sum-net/src/lib.rs` | (Phase 3) Add option to create outbound-only SumNet without listening. |
| `crates/sum-store/src/lib.rs` | (Phase 2) Add `cleanup()` method to delete all local chunks after successful upload. |

---

## Test Plan

1. `ingest_exits_after_confirmation` — ingest a file with 3 listening nodes, verify process exits after R=3 confirmations (does NOT enter serve loop)
2. `ingest_timeout_exits_with_error` — ingest with no reachable nodes, verify process exits with error after upload timeout
3. `ingest_partial_confirmation_warning` — ingest with 2 of 3 nodes reachable, verify process reports partial result
4. `client_mode_no_serve` — run `sum-node --client ingest`, verify the node does NOT respond to ShardRequested events
5. `client_mode_no_por` — run `sum-node --client listen` (should error: "listen requires node mode, not client mode")
6. `download_exits_after_assembly` — already works, just verify explicitly
