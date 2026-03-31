# Phase 4 Execution Plan: Download Command, Garbage Collection & Resilient Upload

## Context

Phases 1-3 are complete (77 tests passing). The storage node can chunk files, build Merkle trees, serve chunks via P2P, enforce ACLs, respond to PoR challenges, and auto-fetch assigned chunks via MarketSync. However, three critical gaps remain:

1. **No download command** — `fetch` only grabs one chunk by CID. There's no way to retrieve a complete file by merkle_root (Step 8 of the README protocol).
2. **No garbage collection** — nodes hold all chunks forever, even after assignment changes. Disk grows without bound.
3. **Single-node upload** — `ingest` pushes to 1 peer only. If that peer drops data before replication, the file is lost.

This plan implements all three as Phase 4 Objectives 1-2 from `PHASE4-EXECUTION-PLAN.md`.

---

## Execution Order

```
Phase A: Foundation (enables both Download and GC)
  A1. ChunkStore: add list_all_cids() + delete()
  A2. ShardRequest: add push_data field
  A3. serve.rs: add handle_push_request()

Phase B: Download Command (highest priority, immediately testable)
  B1. download.rs: DownloadOrchestrator
  B2. main.rs: Download subcommand + run_download()
  B3. Tests: 14 download tests

Phase C: Garbage Collection
  C1. gc.rs: GarbageCollector with mark_and_sweep()
  C2. market_sync.rs: integrate GC after sync_cycle()
  C3. main.rs: --gc-grace-secs flag
  C4. Tests: 9 GC tests

Phase D: Push Protocol + Upload Orchestrator
  D1. sum-net/lib.rs: push_chunk() method
  D2. upload.rs: UploadOrchestrator
  D3. main.rs: modify ingest to use UploadOrchestrator
  D4. Tests: 13 upload tests
```

---

## Phase A: Foundation Changes

### A1. Add `list_all_cids()` and `delete()` to ChunkStore

**File:** `crates/sum-store/src/store.rs`

```rust
pub fn list_all_cids(&self) -> Result<Vec<String>>
// Read chunk_dir, filter *.chunk files, strip extension, return CID strings

pub fn delete(&self, cid: &str) -> Result<bool>
// Remove <cid>.chunk from disk. Returns Ok(true) if existed, Ok(false) if not.
```

Tests: `list_all_cids_empty`, `list_all_cids_populated`, `delete_existing`, `delete_nonexistent`

### A2. Add `push_data` field to ShardRequest

**File:** `crates/sum-net/src/codec.rs`

```rust
pub struct ShardRequest {
    pub cid: String,
    pub offset: Option<u64>,
    pub max_bytes: Option<u64>,
    #[serde(default)]
    pub push_data: Option<Vec<u8>>,  // NEW: when present, this is a push request
}
```

Update all existing `ShardRequest` construction sites (in `lib.rs`, `swarm.rs`, `fetch.rs`) to include `push_data: None`. Update codec round-trip tests.

### A3. Add push request handler to serve.rs

**File:** `crates/sum-store/src/serve.rs`

Update `handle_request()` dispatch to detect `push_data.is_some()` and route to new `handle_push_request()`:
- Verify CID: `blake3(push_data) == cid` via `verify::verify_cid()`
- If invalid: respond with error, do NOT write to disk
- If valid: `store.put(cid, data)` (idempotent — skip if already exists)
- Respond with ACK (empty data, no error)

---

## Phase B: Download Command

### B1. Create DownloadOrchestrator

**New file:** `crates/sum-node/src/download.rs`

```rust
pub struct DownloadOrchestrator {
    merkle_root_hex: String,
    output_path: PathBuf,
    rpc: Arc<L1RpcClient>,
    max_concurrent: usize,       // default 10
    timeout: Duration,            // default 300s
}

pub struct DownloadResult {
    pub chunks_fetched: u32,
    pub chunks_skipped: u32,      // already on disk
    pub total_bytes: u64,
    pub merkle_verified: bool,
}
```

**State machine:**

1. **DiscoveringPeers** — wait for mDNS `PeerDiscovered`, then request manifest via `net.request_manifest(peer_id, merkle_root_hex)`
2. **AwaitingManifest** — wait for `ShardReceived` with `cid.starts_with("manifest:")`, CBOR-deserialize into `DataManifest`. On failure, retry with next peer
3. **FetchingChunks** — for each chunk not already on disk, fetch from assigned nodes (compute assignment via `rpc.get_active_nodes()` + `compute_chunk_assignment()`). Cap at `max_concurrent` in-flight. On failure, try alternate holder. Uses its own `FetchManager` (not shared with SumStore's)
4. **Assembling** — read chunks 0..C-1 from ChunkStore in order, concatenate, write to `output_path`. Optionally verify merkle root by rebuilding tree from chunk hashes

**Key design decisions:**
- Owns its own `FetchManager` to avoid lock contention with the listen loop's SumStore
- `FetchManager::on_chunk_received()` takes `&ChunkStore` (not `&mut`), so a read lock on `Arc<RwLock<SumStore>>` suffices for CID verification + disk write
- Peer rotation on failure: if assigned node N3 fails for chunk 2, try N7 or N1 (other holders from assignment)

**Reuses existing code:**
- `FetchManager` at `crates/sum-store/src/fetch.rs` — windowed chunk download + CID verification
- `SumNet::request_manifest()` at `crates/sum-net/src/lib.rs:107` — manifest request
- `compute_chunk_assignment()` at `crates/sum-store/src/assignment.rs` — find chunk holders
- `MerkleTree::build()` at `crates/sum-store/src/merkle.rs` — root verification
- `manifest::read_manifest()` / CBOR deserialization at `crates/sum-store/src/manifest.rs`

### B2. Add Download subcommand to CLI

**File:** `crates/sum-node/src/main.rs`

Add to Command enum:
```rust
Download {
    merkle_root: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value = "10")]
    max_concurrent: usize,
    #[arg(long, default_value = "300")]
    download_timeout_secs: u64,
},
```

Add `run_download()` function — creates SumNet, SumStore, L1RpcClient, peer_addresses map, instantiates DownloadOrchestrator, runs it, reports result, shuts down.

**File:** `crates/sum-node/src/lib.rs` — add `pub mod download;`

### B3. Download Tests (14 tests)

*Happy path:*
1. `download_full_file` — 5-chunk file, fetch all, reassemble, byte-identical to original
2. `download_single_chunk_file` — C=1, < 1 MB
3. `download_empty_file` — C=0, writes 0-byte file
4. `download_merkle_root_verified` — after reassembly, computed root matches requested root

*Failure + recovery:*
5. `download_node_offline_fallback` — assigned node offline, falls back to alternate holder
6. `download_corrupted_chunk_rejected` — bad blake3 -> reject -> re-fetch from alternate
7. `download_partial_resume` — chunks 0-2 already on disk, only fetches 3-4
8. `download_manifest_not_found` — no peer has manifest -> clear error
9. `download_no_peers_discovered` — no mDNS peers within timeout -> error
10. `download_all_holders_offline` — all R=3 holders for a chunk offline -> error naming missing chunk

*Access control:*
11. `download_acl_authorized` — Bob in access_list -> success
12. `download_acl_unauthorized` — Carol not in access_list -> rejected
13. `download_acl_public` — empty access_list -> anyone can download

*Concurrency:*
14. `download_parallel_fetch` — 10 chunks concurrent, no race conditions

---

## Phase C: Garbage Collection

### C1. Create GarbageCollector

**New file:** `crates/sum-store/src/gc.rs`

```rust
pub struct GarbageCollector {
    unassigned_since: HashMap<String, Instant>,  // in-memory only, resets on restart
    grace_period: Duration,
}

pub struct GcResult {
    pub chunks_deleted: u32,
    pub bytes_freed: u64,
    pub chunks_retained: u32,  // unassigned but within grace period
}
```

**Core method:** `mark_and_sweep(&mut self, store: &ChunkStore, assigned_cids: &HashSet<String>, last_l1_poll: Instant) -> Result<GcResult>`

Logic:
1. If `last_l1_poll` > 5 min ago -> skip GC entirely (stale state), return empty result
2. `store.list_all_cids()` -> enumerate all chunks on disk
3. For each CID not in `assigned_cids`:
   - Track in `unassigned_since` if not already (mark)
   - If unassigned longer than `grace_period` -> `store.delete(cid)` (sweep)
4. Remove from `unassigned_since` any CIDs that are now in `assigned_cids` (re-assigned)
5. Log every deletion with CID and duration

**Design:** `unassigned_since` is in-memory only. On restart, grace period resets — this is the safe/conservative choice.

**File:** `crates/sum-store/src/lib.rs` — add `pub mod gc;`

### C2. Integrate GC into MarketSyncWorker

**File:** `crates/sum-node/src/market_sync.rs`

Add fields to `MarketSyncWorker`:
```rust
gc: GarbageCollector,
last_l1_poll: Instant,
```

Add helper: `compute_assigned_cids(&self, files, node_addrs, store) -> HashSet<String>` — iterates all funded files, computes assignment, collects CIDs for chunks assigned to this node.

At end of `sync_cycle()`: update `last_l1_poll`, call `gc.mark_and_sweep(store.local, &assigned_cids, self.last_l1_poll)`.

### C3. CLI flag

**File:** `crates/sum-node/src/main.rs`

Add `--gc-grace-secs` (default 3600, env `SUM_GC_GRACE`). Pass to `MarketSyncWorker::new()`.

### C4. GC Tests (9 tests)

*Core logic:*
15. `gc_assigned_chunk_not_deleted` — chunk is assigned -> stays on disk
16. `gc_unassigned_within_grace` — unassigned 30 min (< 1 hr grace) -> NOT deleted
17. `gc_unassigned_past_grace` — unassigned 2 hours -> deleted, logged
18. `gc_reassigned_during_grace` — unassigned, then re-assigned before grace expires -> deletion cancelled

*Safety:*
19. `gc_l1_unreachable_paused` — last poll > 5 min -> GC skipped, nothing deleted
20. `gc_multiple_files` — assigned to files A,C but not B -> only B's chunks deleted
21. `gc_fee_pool_exhausted` — file unfunded but still on-chain -> NOT deleted (assignment-based, not funding-based)
22. `gc_race_with_serve` — chunk deleted only if no active transfer (or: deletion is atomic via fs::remove_file, serve uses mmap which holds fd)
23. `gc_disk_space_reported` — GcResult.bytes_freed matches actual freed bytes

---

## Phase D: Push Protocol + Upload Orchestrator

### D1. Add push_chunk() to SumNet

**File:** `crates/sum-net/src/lib.rs`

```rust
pub async fn push_chunk(&self, peer_id: PeerId, cid: String, data: Vec<u8>) -> Result<()>
// Sends ShardRequest { cid, offset: None, max_bytes: None, push_data: Some(data) }
// via existing SwarmCommand::RequestShard
```

### D2. Create UploadOrchestrator

**New file:** `crates/sum-node/src/upload.rs`

```rust
pub struct UploadOrchestrator {
    rpc: Arc<L1RpcClient>,
    l1_address: [u8; 20],
    timeout: Duration,
}

pub struct UploadResult {
    pub confirmed: u32,
    pub total: u32,
    pub timeout: bool,
    pub failed_chunks: Vec<UploadFailure>,
}
```

Logic:
1. `rpc.get_active_nodes()` -> compute assignment
2. Build L1 addr -> PeerId reverse map from peer_addresses
3. For each chunk x each of R=3 assigned nodes: `net.push_chunk(peer_id, cid, data)`
4. Track confirmations: ACK responses (ShardReceived with empty data + no error) AND gossipsub ChunkAnnouncements matching merkle_root from target PeerIds
5. After timeout: report UploadResult with failed_chunks detail

**File:** `crates/sum-node/src/lib.rs` — add `pub mod upload;`

### D3. Modify ingest to use UploadOrchestrator

**File:** `crates/sum-node/src/main.rs`

Change `run_ingest()`:
1. Ingest file locally (existing)
2. Wait for peer discovery (existing)
3. Create UploadOrchestrator, push to R=3 nodes (NEW)
4. Announce via gossipsub (existing)
5. Report result, exit or stay serving

Add `--upload-timeout` flag (default 120s) to Ingest variant.

### D4. Upload Tests (13 tests)

*Push protocol:*
24. `push_chunk_accepted` — valid CID -> stored, ACK returned
25. `push_chunk_invalid_cid` — bad hash -> rejected, NOT stored
26. `push_chunk_duplicate_idempotent` — push same CID twice -> accepted, 1 copy on disk
27. `push_chunk_oversized_rejected` — > 256 MiB -> rejected

*Upload orchestrator:*
28. `upload_all_confirmed` — R=3 nodes, all confirm -> UploadResult { confirmed: 30, total: 30 }
29. `upload_one_unreachable_fallback` — 1 node down -> probes next -> still R confirmations
30. `upload_all_unreachable_error` — all nodes down -> error with failed chunk list
31. `upload_partial_timeout` — 2 of 3 confirmations -> partial result, timeout: true
32. `upload_wrong_peer_ignored` — announcement from non-target PeerId -> not counted
33. `upload_wrong_root_ignored` — announcement with wrong merkle_root -> not counted
34. `upload_node_silent_drop` — node stores but doesn't announce -> detected via timeout
35. `upload_assignment_matches_network` — orchestrator's assignment matches nodes' independent computation

*Integration:*
36. `integration_upload_3_nodes` — 3 processes, push 3-chunk file, verify all hold assigned chunks

---

## New Files

| File | Purpose |
|------|---------|
| `crates/sum-node/src/download.rs` | DownloadOrchestrator — full file retrieval by merkle_root |
| `crates/sum-store/src/gc.rs` | GarbageCollector — mark-and-sweep unassigned chunks |
| `crates/sum-node/src/upload.rs` | UploadOrchestrator — multi-node push with confirmation |

## Modified Files

| File | Changes |
|------|---------|
| `crates/sum-store/src/store.rs` | Add `list_all_cids()`, `delete()` |
| `crates/sum-net/src/codec.rs` | Add `push_data: Option<Vec<u8>>` to ShardRequest |
| `crates/sum-store/src/serve.rs` | Add `handle_push_request()`, update dispatch |
| `crates/sum-net/src/lib.rs` | Add `push_chunk()` method |
| `crates/sum-node/src/main.rs` | Add Download/upload CLI variants, --gc-grace-secs, run_download() |
| `crates/sum-node/src/lib.rs` | Add `pub mod download; pub mod upload;` |
| `crates/sum-node/src/market_sync.rs` | Add GC field, compute_assigned_cids(), run GC after sync |
| `crates/sum-store/src/lib.rs` | Add `pub mod gc;` |

---

## Verification

1. `cargo test --workspace` — all 77 existing tests + 36 new tests pass (113 total)
2. **Manual download test:** Terminal A runs `listen`, Terminal B runs `ingest <file>`, Terminal C runs `download <merkle_root> --output /tmp/out.pdf` with `HOME=/tmp/node_c`. Verify `/tmp/out.pdf` is byte-identical to original via `diff` or `sha256sum`
3. **P2P integration test update:** Extend `test_p2p.sh` to include download + reassembly verification
4. **GC manual test:** Ingest a file, stop node, manually delete manifest for that file, restart with short grace period (--gc-grace-secs 10), verify chunks are deleted after ~10s