# Phase 3: Combinatorial Assignment — Test Details

**New tests in Phase 3:** 12 (in `assignment.rs`)
**Total tests:** 77 (65 from Phases 1-2 + 12 new)
**Result:** ALL PASS

---

## assignment.rs (12 tests)

**Source:** `crates/sum-store/src/assignment.rs`

These tests validate the deterministic chunk-to-node assignment algorithm that both the L1 and off-chain nodes use to agree on who stores what.

### 1. `basic_assignment_3_replicas`
5 nodes, 4-chunk file, R=3 — each chunk gets exactly 3 assigned nodes.

### 2. `no_duplicate_nodes_per_chunk`
10 nodes, 8 chunks, R=3 — no chunk has the same node assigned twice (linear probing works).

### 3. `deterministic`
Same inputs produce identical output across calls — nodes must agree on assignments.

### 4. `different_files_different_assignments`
Two files with different merkle_roots get different node assignments — prevents hotspots.

### 5. `fewer_nodes_than_replication`
2 nodes, R=3 — each chunk gets 2 nodes (min(R, N) = 2). No panic.

### 6. `single_node`
1 node, R=3 — each chunk gets 1 node. Degraded but safe.

### 7. `empty_nodes`
0 nodes — empty assignment vectors. No panic.

### 8. `zero_chunks`
0-chunk file — empty assignment. No panic.

### 9. `chunks_for_node_helper`
10 nodes, 10 chunks — every node gets at least one assigned chunk (no node has zero work).

### 10. `nodes_for_chunk_helper`
Verifies the convenience function correctly indexes into the assignment. Out-of-bounds returns None.

### 11. `large_file_even_distribution`
100 chunks, 10 nodes, R=3 = 300 slots. Verifies no node gets 0 assignments and no node gets all 100 (distribution is reasonably even).

### 12. `cross_verify_hash_computation`
Manually computes the blake3 hash that the L1 would compute and verifies our hasher produces identical output. This is the critical L1 alignment test — if the hash inputs differ by even one byte, assignments would diverge.

---

## Raw Test Output

```
running 12 tests
test assignment::tests::empty_nodes ... ok
test assignment::tests::cross_verify_hash_computation ... ok
test assignment::tests::basic_assignment_3_replicas ... ok
test assignment::tests::fewer_nodes_than_replication ... ok
test assignment::tests::different_files_different_assignments ... ok
test assignment::tests::deterministic ... ok
test assignment::tests::chunks_for_node_helper ... ok
test assignment::tests::nodes_for_chunk_helper ... ok
test assignment::tests::zero_chunks ... ok
test assignment::tests::single_node ... ok
test assignment::tests::no_duplicate_nodes_per_chunk ... ok
test assignment::tests::large_file_even_distribution ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured
```

---

## L1 Changes (sum-chain repo — not tested via cargo test here)

The following L1 changes were made and compile successfully:

| File | Change |
|------|--------|
| `primitives/src/storage_metadata.rs` | Added `REPLICATION_FACTOR = 3` constant |
| `primitives/src/lib.rs` | Re-exported `REPLICATION_FACTOR` |
| `state/src/storage_metadata.rs` | Added `compute_chunk_assignment()` function |
| `state/src/storage_metadata.rs` | Modified `generate_challenge()` to use assignment-based target selection |
| `rpc/src/api.rs` | Added `storage_getActiveNodes` trait method |
| `rpc/src/server.rs` | Implemented `storage_get_active_nodes()` handler |

These L1 changes should be tested via `cargo test` in the sum-chain repo separately.
