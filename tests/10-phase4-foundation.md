# Phase 4 Foundation Tests

## Overview

Phase 4 foundation changes add chunk enumeration/deletion to ChunkStore, push_data support to the wire protocol, and push request handling in serve.rs.

**Test count:** 12 new tests (4 ChunkStore + 1 codec + 7 GC)

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

## Test Output

```
running 7 tests  (sum-store: store)      test result: ok. 7 passed
running 5 tests  (sum-net: codec)        test result: ok. 5 passed
running 7 tests  (sum-store: gc)         test result: ok. 7 passed

Total: 89 tests passing across workspace (77 prior + 12 new)
```
