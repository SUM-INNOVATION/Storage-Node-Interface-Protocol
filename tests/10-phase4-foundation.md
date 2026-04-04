# Phase 4 Foundation Tests

## Overview

Phase 4 foundation changes add chunk enumeration/deletion to ChunkStore, push_data support to the wire protocol, and push request handling in serve.rs.

**Test count:** 19 new tests (4 ChunkStore + 1 codec + 7 GC + 1 cleanup + 2 health_check + 4 metrics)

---

## sum-store: ChunkStore Tests (4 new)

### `list_all_cids_empty`
- **What:** Call `list_all_cids()` on a fresh, empty ChunkStore
- **Expected:** Returns empty Vec
- **Result:** PASS

### `list_all_cids_populated`
- **What:** Put 3 chunks ("bafaaa", "bafbbb", "bafccc"), call `list_all_cids()`
- **Expected:** Returns sorted Vec of 3 CID strings
- **Result:** PASS

### `delete_existing`
- **What:** Put chunk "bafdelete", verify it exists, call `delete("bafdelete")`
- **Expected:** Returns `Ok(true)`, chunk no longer on disk
- **Result:** PASS

### `delete_nonexistent`
- **What:** Call `delete("bafnope")` on empty store
- **Expected:** Returns `Ok(false)`, no error
- **Result:** PASS

---

## sum-net: Codec Tests (1 new)

### `push_request_round_trip`
- **What:** Create ShardRequest with `push_data: Some(vec![0xDE; 8192])`, write via codec, read back
- **Expected:** Decoded request has matching CID, push_data matches original bytes
- **Result:** PASS

---

## sum-store: GC Tests (7 new)

### `gc_assigned_chunk_not_deleted`
- **What:** Chunk "cid_a" is on disk and in `assigned_cids` set. Run GC with 0s grace.
- **Expected:** Chunk remains on disk, 0 deletions
- **Result:** PASS

### `gc_unassigned_within_grace`
- **What:** Chunk "cid_b" is on disk but NOT assigned. GC grace = 3600s (1 hour).
- **Expected:** Chunk retained (within grace), `chunks_retained = 1`
- **Result:** PASS

### `gc_unassigned_past_grace`
- **What:** Chunk "cid_c" is unassigned. GC grace = 0s (immediate).
- **Expected:** Chunk deleted, `bytes_freed > 0`
- **Result:** PASS

### `gc_reassigned_during_grace`
- **What:** First sweep: "cid_d" unassigned, starts grace. Second sweep: "cid_d" re-assigned.
- **Expected:** Second sweep cancels pending deletion, `tracked_count = 0`, chunk on disk
- **Result:** PASS

### `gc_l1_unreachable_paused`
- **What:** `last_l1_poll` is 10 minutes ago (> 5 min threshold). GC grace = 0s.
- **Expected:** GC skipped entirely, `result.skipped = true`, chunk NOT deleted
- **Result:** PASS

### `gc_multiple_files`
- **What:** 3 chunks from files A, B, C. Node assigned to A and C only.
- **Expected:** Only file B's chunk deleted. A and C chunks remain.
- **Result:** PASS

### `gc_disk_space_reported`
- **What:** 4096-byte chunk, unassigned, GC grace = 0s.
- **Expected:** `bytes_freed = 4096` after deletion
- **Result:** PASS

---

## sum-store: Health Check Tests (2 new)

### `health_check_empty_store`
- **What:** Create empty SumStore, call health_check()
- **Expected:** chunk_count=0, manifest_count=0, disk_usage < 1KB, store_dir_writable=true
- **Result:** PASS

### `health_check_with_chunks`
- **What:** Create SumStore, put 2 chunks (11 bytes + 4096 bytes), call health_check()
- **Expected:** chunk_count=2, disk_usage > 0, store_dir_writable=true
- **Result:** PASS

---

## sum-node: Metrics Tests (4 new)

### `metrics_increment_and_snapshot`
- **What:** Create NodeMetrics, increment chunks_served(3), por_submitted(2), por_failed(1), gc_deleted(5), call snapshot()
- **Expected:** Snapshot values match: 3, 2, 1, 5, 0
- **Result:** PASS

### `metrics_peers_inc_dec`
- **What:** inc_peers 3 times, dec_peers once
- **Expected:** peers_connected = 2
- **Result:** PASS

### `metrics_default_is_zero`
- **What:** Fresh NodeMetrics::default(), call snapshot()
- **Expected:** All counters are 0
- **Result:** PASS

### `metrics_gc_batch_increment`
- **What:** inc_gc_deleted(10) then inc_gc_deleted(25)
- **Expected:** gc_chunks_deleted = 35
- **Result:** PASS

---

## Test Output

```
running 14 tests  (sum-net)              test result: ok. 14 passed
running 8 tests   (sum-node)             test result: ok.  8 passed
running 66 tests  (sum-store)            test result: ok. 66 passed
running 8 tests   (sum-types)            test result: ok.  8 passed

Total: 96 tests passing across workspace (77 prior + 19 new Phase 4)
```
