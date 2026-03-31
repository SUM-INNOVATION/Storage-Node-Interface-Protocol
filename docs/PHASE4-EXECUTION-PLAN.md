# Phase 4 Execution Plan: Scale-Out — Reed-Solomon, WAN Discovery & Production Hardening

## Overview

Phase 4 has five objectives:
1. **Resilient Upload — Multi-Node Push + Confirmation** — eliminate the single-point-of-failure window between upload and replication
2. **File Download Command + Garbage Collection** — complete the retrieval workflow (Step 8) and reclaim disk from unassigned chunks
3. **Reed-Solomon Erasure Coding** — replace 3x full replication with coded redundancy (3x -> ~1.5x overhead)
4. **WAN Discovery** — Kademlia DHT + NAT traversal so nodes work beyond LAN
5. **Production Hardening** — metrics, graceful shutdown, logging, full E2E test against live validators

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

**Tests (target: 13 tests):**

*Push protocol (unit):*
1. `push_chunk_accepted` — node receives pushed chunk, blake3 hash matches CID, writes to disk, returns ACK
2. `push_chunk_invalid_cid` — pushed data's blake3 hash does not match claimed CID, node rejects with error, does NOT write to disk
3. `push_chunk_duplicate_idempotent` — push same CID twice, node accepts both silently (idempotent), only 1 copy on disk
4. `push_chunk_oversized_rejected` — pushed data exceeds max message size (256 MiB), node rejects

*Upload orchestrator (unit):*
5. `upload_all_confirmed` — push to R=3 nodes, all 3 announce via gossipsub within timeout, UploadResult { confirmed: 30, total: 30, timeout: false } for C=10 chunks
6. `upload_one_unreachable_fallback` — 1 of 3 assigned nodes unreachable, orchestrator probes next node in sorted list, still achieves R=3 confirmations per chunk
7. `upload_all_unreachable_error` — all R=3 assigned nodes AND all fallback nodes unreachable, returns error with list of failed chunks and nodes
8. `upload_partial_timeout` — 2 of 3 confirmations received before timeout, returns partial result with explicit list of unconfirmed chunks/nodes, timeout: true
9. `upload_wrong_peer_announcement_ignored` — gossipsub announcement from a PeerId that wasn't a push target is not counted as a confirmation
10. `upload_wrong_merkle_root_ignored` — gossipsub announcement with matching PeerId but wrong merkle_root is not counted
11. `upload_node_accepts_but_silent` — node receives push, writes to disk, but does NOT announce (gossipsub failure), orchestrator detects via timeout on that specific chunk/node
12. `upload_assignment_matches_network` — orchestrator's computed assignment for a file matches what nodes independently compute from the same on-chain state (cross-validate with assignment.rs)

*Integration:*
13. `integration_upload_3_nodes` — spawn 3 sum-node processes, Alice pushes a 3-chunk file, verify all 3 nodes hold their assigned chunks on disk, verify gossipsub announcements received

---

## Objective 2: File Download Command + Garbage Collection

**2a — Download command (Step 8 of the protocol, currently not implemented):**

There is no way to retrieve a complete file from the network. The `fetch` subcommand only downloads a single chunk by CID. Bob cannot say "give me file `34a749...`" and get back the original PDF.

**What's needed:** `sum-node download <merkle_root_hex> --output <path>`

| File | Action | What |
|------|--------|------|
| `sum-node/src/download.rs` | **New** | `DownloadOrchestrator` — takes a merkle_root, requests the DataManifest from a peer, fetches all C chunks in parallel from multiple nodes (using the assignment to know who holds what), verifies each chunk's CID, concatenates in order, writes to `--output` path |
| `sum-node/src/main.rs` | **Modify** | Add `download` subcommand: `sum-node download <merkle_root> --output ./file.pdf`. Requires `--rpc-url` for ACL check (optional — nodes enforce ACL, not the client) |

**Design details:**
- Request manifest from any discovered peer via `"manifest:<merkle_root_hex>"`.
- Compute assignment via `storage_getActiveNodes()` RPC to know which nodes hold which chunks — fetch from assigned nodes preferentially.
- Fall back to any peer that announced the chunk via gossipsub if assigned nodes are unreachable.
- Fetch chunks in parallel (up to 10 concurrent requests) for performance.
- Verify each chunk: `blake3(received_bytes)` must match the CID in the manifest.
- After all C chunks verified, concatenate chunk 0 through chunk C-1 and write to output file.
- Optionally verify the full file: build Merkle tree from chunk hashes, confirm root matches the requested merkle_root.

**2b — Garbage Collection (currently missing from the plan entirely):**

When nodes join or leave the network, the assignment recomputes (because the modulus changes). A node that was assigned chunk 4 may no longer be assigned to it after a new node registers. Currently, that node holds the unassigned chunk indefinitely, wasting disk space.

| File | Action | What |
|------|--------|------|
| `sum-store/src/gc.rs` | **New** | `GarbageCollector` — iterates all chunks on disk, recomputes the current assignment using on-chain state, deletes chunks this node is no longer assigned to. Respects a grace period (e.g., keep unassigned chunks for 1 hour before deleting, in case of transient node list changes) |
| `sum-node/src/market_sync.rs` | **Modify** | After each sync cycle, run GC. Only delete if the chunk has been unassigned for longer than `gc_grace_period` |
| `sum-node/src/main.rs` | **Modify** | Add `--gc-grace-secs` flag (default 3600 = 1 hour, env SUM_GC_GRACE) |

**Design details:**
- GC never deletes a chunk that this node is currently assigned to.
- Grace period prevents thrashing: if a node briefly leaves and rejoins, assignments temporarily shift and shift back. Without a grace period, chunks would be deleted and immediately re-fetched.
- GC logs every deletion: `gc: deleting unassigned chunk cid=bafkr4i... (unassigned for 3612s)`.
- GC runs after MarketSync, not independently — it uses the same assignment computation.
- Safety: GC only runs when the node has successfully polled the L1 within the last 5 minutes. If the L1 is unreachable, GC is paused (we don't want to delete based on stale state).

**Tests (target: 19 tests):**

*Download — happy path (unit):*
1. `download_full_file` — request manifest from peer, fetch all C=5 chunks, verify each CID, concatenate, written file is byte-identical to original
2. `download_single_chunk_file` — file with C=1 (< 1 MB), download produces identical file
3. `download_empty_file` — file with C=0 chunks, writes empty 0-byte file
4. `download_merkle_root_verified` — after reassembly, build Merkle tree from downloaded chunk hashes, computed root matches requested merkle_root

*Download — failure + recovery (unit):*
5. `download_node_offline_fallback` — assigned node for chunk 3 is offline, orchestrator falls back to another node holding chunk 3, download completes
6. `download_corrupted_chunk_rejected` — received chunk's blake3 hash doesn't match CID, chunk is rejected, re-fetched from alternate node, download completes with valid data
7. `download_partial_resume` — chunks 0-2 already on disk from prior incomplete download, orchestrator skips them, only fetches chunks 3-4, assembles complete file
8. `download_manifest_not_found` — no peer has the manifest for the requested merkle_root, returns clear error with "manifest not found" message
9. `download_no_peers_discovered` — no peers found via mDNS within timeout, returns error with "no peers" message
10. `download_all_holders_offline` — all R=3 nodes holding chunk 2 are offline, download fails with explicit error naming the missing chunk and its CID

*Download — access control (unit):*
11. `download_acl_authorized` — file has access_list=[Bob], Bob requests chunks, serving nodes check ACL, Bob receives all chunks
12. `download_acl_unauthorized` — file has access_list=[Bob], Carol requests chunks, serving node denies with ACL error, Carol receives nothing
13. `download_acl_public` — file has access_list=[] (empty), any client can download

*Download — concurrency:*
14. `download_parallel_fetch` — 10 chunks fetched concurrently from different nodes, all arrive and verify, no race conditions in disk writes

*Garbage collection — core logic (unit):*
15. `gc_assigned_chunk_not_deleted` — node is assigned to chunk, GC runs, chunk remains on disk
16. `gc_unassigned_within_grace_period` — node is no longer assigned to chunk, but unassigned for only 30 minutes (< 1 hour grace), GC does NOT delete
17. `gc_unassigned_past_grace_period` — node unassigned for 2 hours (> 1 hour grace), GC deletes the chunk, logs the deletion with CID and duration
18. `gc_reassigned_during_grace` — node is unassigned, grace period ticking, then node list changes and node is re-assigned, GC cancels the pending deletion

*Garbage collection — safety (unit):*
19. `gc_l1_unreachable_paused` — L1 RPC is unreachable (last successful poll > 5 minutes ago), GC does NOT run, no chunks deleted, warning logged
20. `gc_multiple_files` — node holds chunks from 3 different files, is assigned to files A and C but not B, GC only deletes file B's chunks (past grace), files A and C untouched
21. `gc_fee_pool_exhausted` — file's fee_pool is 0 (no longer funded), node still holds chunks, GC does NOT delete (the data still exists on-chain even if unfunded — deletion is only based on assignment, not funding)
22. `gc_race_with_serve` — GC marks a chunk for deletion, but a peer requests that chunk mid-GC, serve completes before deletion (GC should not delete chunks with active transfers)
23. `gc_disk_space_reported` — GC logs total bytes freed after each run

*Integration:*
24. `integration_download_reassemble` — spawn 3 nodes, ingest a 5-chunk file on node A, download by merkle_root on a clean node, verify output file matches original byte-for-byte

---

## Objective 3: Reed-Solomon Erasure Coding

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

**Tests (target: 18 tests):**

*Reed-Solomon encoder/decoder (unit):*
1. `rs_encode_decode_round_trip` — encode 1 MB chunk into k=4 data + m=2 parity shards, decode from all 6 shards, output is byte-identical to input
2. `rs_decode_1_missing` — remove any 1 of 6 shards, decode from remaining 5, output is byte-identical
3. `rs_decode_2_missing` — remove any 2 of 6 shards (all 15 combinations), decode from remaining 4, all produce byte-identical output
4. `rs_decode_3_missing_fails` — remove 3 shards (only 3 remain, < k=4), decode returns error
5. `rs_decode_corrupted_shard` — 1 shard has corrupted bytes (wrong blake3), detected via CID check, treated as missing, reconstruct from remaining 5
6. `rs_decode_1_corrupted_1_missing` — 1 corrupted + 1 missing = 2 effective losses, still recoverable (4 valid shards remain)
7. `rs_encode_last_chunk_smaller` — file's last chunk is 500 KB (< 1 MB), RS encode pads correctly, decode produces original 500 KB (not padded output)
8. `rs_shard_hashes_deterministic` — same input chunk always produces same 6 shard hashes
9. `rs_different_chunks_different_shards` — two different chunks produce completely different shard hashes

*Merkle tree with shards (unit):*
10. `merkle_tree_shard_leaves` — build tree from shard hashes (6 leaves per chunk, 60 total for 10-chunk file), verify root is deterministic
11. `merkle_proof_shard_index` — generate proof for shard index 17 (chunk 2, shard 5), verify against L1 algorithm using (shard_index >> level) & 1
12. `merkle_proof_all_shards_cross_validate` — for a 3-chunk file (18 shards), generate and verify proofs for all 18 shard indices against L1 verification algorithm

*Assignment with shards (unit):*
13. `shard_assignment_6_per_chunk` — 10 chunks × 6 shards = 60 shards, each assigned to 1 node (not replicated), verify 60 assignments across N=10 nodes
14. `shard_assignment_even_distribution` — over 100 chunks (600 shards), verify no node holds more than 15% deviation from the mean
15. `shard_assignment_no_duplicate_node_per_chunk` — for each chunk's 6 shards, verify all 6 are assigned to different nodes (when N >= 6)

*DataManifest with shards (unit):*
16. `manifest_with_shards_cbor_round_trip` — DataManifest containing ShardDescriptors serializes to CBOR and deserializes identically
17. `manifest_with_shards_json_export` — JSON export contains shard_index, parent_chunk_index, is_parity fields

*End-to-end:*
18. `e2e_ingest_encode_prove_verify` — ingest file, RS encode all chunks, build shard-level Merkle tree, generate proof for random shard, verify proof against L1 algorithm, verify shard hash matches CID

---

## Objective 4: WAN Discovery (Kademlia DHT + NAT Traversal)

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

**Tests (target: 14 tests):**

*TCP/Noise transport (unit):*
1. `tcp_noise_handshake_loopback` — two peers connect via TCP/Noise on localhost, handshake completes, PeerIds match expected
2. `tcp_noise_message_round_trip` — send and receive a message over TCP/Noise transport, data integrity verified
3. `quic_and_tcp_simultaneous` — node listens on both QUIC and TCP, peer connects via TCP when QUIC is blocked, falls back correctly

*Kademlia DHT (unit):*
4. `kademlia_add_address_empty_peer_list` — bootstrap with no known peers, does not panic, returns empty routing table
5. `kademlia_add_address_and_find` — add 3 peers to routing table, query for a known PeerId, returns correct addresses
6. `kademlia_bootstrap_populates_routing` — bootstrap with 1 known peer, DHT populates with that peer's known peers (transitive discovery)

*Capability advertisement (unit):*
7. `capability_announcement_encode_decode` — CapabilityAnnouncement with PeerId, L1 address, listen addresses, disk space serializes and deserializes via bincode
8. `capability_announcement_correct_l1_address` — announcement's L1 address matches blake3(pubkey)[12..32] for the node's keypair
9. `capability_announcement_disk_space_accurate` — reported available disk space matches actual store directory free space (within 1 MB tolerance)

*AutoNAT + DCUtR (unit):*
10. `autonat_config_builds` — AutoNAT behaviour config initializes without error with default settings
11. `dcutr_relay_client_config_builds` — DCUtR relay client behaviour initializes without error, accepts relay address list

*Config (unit):*
12. `netconfig_bootstrap_peers_serde` — NetConfig with bootstrap_peers, enable_wan, relay_addrs serializes to JSON and deserializes identically
13. `netconfig_empty_bootstrap_valid` — NetConfig with empty bootstrap_peers and enable_wan=false is valid (LAN-only mode)

*Integration:*
14. `integration_kademlia_discovery_no_mdns` — spawn 3 nodes on different ports: node A is bootstrap, node B and C know only A's address, B and C discover each other via Kademlia DHT (mDNS disabled), verify PeerDiscovered events emitted for all pairs

---

## Objective 5: Production Hardening

**Files to create/modify:**

| File | Action | What |
|------|--------|------|
| `sum-node/src/metrics.rs` | **New** | Prometheus-compatible metrics: chunks_stored, chunks_served, por_challenges_answered, por_challenges_missed, peers_connected, bytes_transferred, assignment_coverage_percent |
| `sum-node/src/main.rs` | **Modify** | Graceful shutdown: on SIGINT/SIGTERM, stop accepting new requests, flush pending PoR proofs, close swarm, write final metrics |
| `sum-store/src/lib.rs` | **Modify** | `SumStore::health_check()` — verifies store dir writable, manifest index loadable, reports chunk count and disk usage |
| `tests/integration/test_e2e_l1.sh` | **Modify** | Run against live validators. Validate full loop: register node -> ingest file -> register on L1 -> wait for challenge -> verify proof accepted -> check Koppa earned |
| `tests/integration/test_wan_discovery.sh` | **New** | Spawn 3 nodes: 1 bootstrap, 2 clients on different ports. Verify Kademlia discovery works without mDNS |
| `tests/integration/test_erasure.sh` | **New** | Ingest file with RS encoding, kill 2 of 6 shard holders, verify file is still reconstructable from remaining 4 |

**Tests (target: 12 tests):**

*Metrics (unit):*
1. `metrics_counter_increments` — serve a chunk, verify chunks_served counter increments by 1
2. `metrics_por_answered_tracked` — submit a PoR proof, verify por_challenges_answered increments
3. `metrics_por_missed_tracked` — let a challenge expire (mock), verify por_challenges_missed increments
4. `metrics_peers_connected_gauge` — connect 2 peers, gauge reads 2, disconnect 1, gauge reads 1
5. `metrics_bytes_transferred_accumulates` — transfer 3 chunks (3 MB), verify bytes_transferred == 3,145,728

*Health check (unit):*
6. `health_check_valid_store` — store dir exists and writable, manifest index loadable, returns healthy with chunk count and disk usage
7. `health_check_missing_dir` — store dir does not exist, returns unhealthy with specific error
8. `health_check_readonly_dir` — store dir exists but not writable, returns unhealthy

*Graceful shutdown (unit):*
9. `shutdown_completes_within_timeout` — send SIGTERM, node flushes pending PoR proofs, closes swarm, exits within 10 seconds
10. `shutdown_mid_transfer` — node is mid-chunk-transfer when SIGTERM arrives, completes the in-flight transfer before shutting down (no corrupt partial writes)

*E2E integration (requires live validators):*
11. `e2e_full_por_loop` — register node, ingest file, register file on L1, start node, wait for PoR challenge (up to 200 blocks), verify proof accepted, verify Koppa balance increased by CHALLENGE_REWARD
12. `e2e_slash_on_missing_proof` — register node, register file, start node but delete a chunk from disk, wait for challenge on that chunk, verify node gets slashed (balance decreased by 5%), status changes to Slashed

---

## Execution Order

```
Step 1: Objective 1 (Resilient Upload) — HIGHEST PRIORITY
  |-- 1a. sum-net: push_chunk() method (proactive send)           [DONE]
  |-- 1b. sum-node/upload.rs: UploadOrchestrator                  [DONE]
  |-- 1c. sum-net/codec.rs: push_data field on ShardRequest       [DONE]
  |-- 1d. sum-store/serve.rs: handle_push_request()               [DONE]
  |-- 1e. sum-node/main.rs: ingest uses UploadOrchestrator        [NOT DONE — still uses old single-peer flow]
  +-- 1f. Tests: 1 of 13 passing (codec push round-trip)

Step 2: Objective 2 (Download + GC) — HIGH PRIORITY
  |-- 2a. sum-node/download.rs: DownloadOrchestrator              [DONE]
  |-- 2b. sum-node/main.rs: download subcommand                   [DONE]
  |-- 2c. sum-store/gc.rs: GarbageCollector                       [DONE]
  |-- 2d. sum-node/market_sync.rs: GC integration after sync      [DONE]
  |-- 2e. sum-node/main.rs: --gc-grace-secs flag                  [DONE]
  |-- 2f. sum-store/store.rs: list_all_cids() + delete()          [DONE]
  +-- 2g. Tests: 11 of 24 passing (4 ChunkStore + 7 GC; download integration tests require live peers)

Step 3: Objective 3 (Reed-Solomon)
  |-- 3a. sum-types: Add erasure constants + ShardDescriptor
  |-- 3b. sum-store/erasure.rs: RS encoder/decoder
  |-- 3c. sum-store/chunker.rs: Integrate RS into chunking pipeline
  |-- 3d. sum-store/assignment.rs: Shard-level assignment
  |-- 3e. sum-node/por_worker.rs + market_sync.rs: Shard-aware
  |-- 3f. L1 changes (sum-chain): Shard-aware challenges + verification
  +-- 3g. Tests: 18 tests (9 RS codec + 3 merkle + 3 assignment + 2 manifest + 1 e2e)

Step 4: Objective 4 (WAN Discovery)
  |-- 4a. sum-net/transport.rs: TCP/Noise fallback
  |-- 4b. sum-net/nat.rs: AutoNAT + DCUtR
  |-- 4c. sum-net/capability.rs: Gossipsub capability ads
  |-- 4d. sum-net/behaviour.rs + swarm.rs: Wire in Kademlia + NAT
  |-- 4e. sum-net/discovery.rs: Kademlia event handling
  |-- 4f. sum-node/main.rs: CLI flags + bootstrap
  +-- 4g. Tests: 14 tests (3 transport + 3 kademlia + 3 capability + 2 NAT + 2 config + 1 integration)

Step 5: Objective 5 (Production Hardening)
  |-- 5a. sum-node/metrics.rs: Prometheus metrics
  |-- 5b. Graceful shutdown + health checks
  |-- 5c. E2E L1 integration test against live validators
  +-- 5d. Tests: 12 tests (5 metrics + 3 health check + 2 shutdown + 2 e2e L1)
```

---

## Total New Tests: 81
## Total Expected Tests After Phase 4: 158

| Objective | Tests | Status |
|-----------|-------|--------|
| 1. Resilient Upload | 13 | Code complete, not wired into ingest CLI. push_data + push handler + UploadOrchestrator built. 1 codec test passing. |
| 2. Download + GC | 24 | Download command DONE. GC DONE. 7 GC tests + 4 ChunkStore tests passing. Download integration tests require live peers. |
| 3. Reed-Solomon | 18 | NOT STARTED |
| 4. WAN Discovery | 14 | NOT STARTED |
| 5. Production Hardening | 12 | NOT STARTED |
| **Phase 4 Total** | **81** | |
| Prior (Phases 1-3) | 77 | All passing |
| Phase 4 implemented so far | 12 | 4 ChunkStore + 1 codec + 7 GC |
| **Current Grand Total** | **89** | All passing |

---

## Current Testability

### Testable Now (no external dependencies)
- `cargo test --workspace` — 89 unit tests
- `bash tests/integration/test_p2p.sh` — 2-node P2P chunk transfer on localhost
- Two-terminal download test: `sum-node ingest <file>` on Terminal A, `sum-node download <merkle_root> --output <path>` on Terminal B (use `HOME=/tmp/dl` for clean store)
- LAN test: same commands across two computers on the same WiFi — mDNS discovers peers automatically
- Tailscale/VPN test: mDNS may work over Tailscale mesh, enabling cross-network testing without WAN support

### Requires Running L1 (sum-chain validators with RPC on port 9944)
- PoR challenge/response loop (Steps 5-7)
- MarketSync auto-fetch of assigned chunks (Step 4)
- ACL enforcement on private files (Step 8b)
- GC with real on-chain assignment state
- Upload orchestrator with real R=3 assignment (needs storage_getActiveNodes)

### Requires WAN Support (Phase 4, Objective 4 — NOT STARTED)
- Two computers on different networks (different WiFi, different cities, over the internet)
- Only peer discovery mechanism is mDNS (local network broadcast only)
- Kademlia DHT + bootstrap nodes required for internet-wide connectivity
- Workaround: Tailscale or similar VPN creates a virtual LAN where mDNS works across physical networks
