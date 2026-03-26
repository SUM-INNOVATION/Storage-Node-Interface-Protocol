# Phase 2 Test Report — L1 RPC Bridge, ACL Enforcement & PoR Responder

**Date:** 2026-03-25
**Rust toolchain:** 1.85+ (2024 edition)
**Command:** `cargo test --workspace`
**Result:** 63 passed, 0 failed (47 from Phase 1 + 16 new)

---

## Summary

### New Tests Added in Phase 2

| Crate | Source File | New Tests | Status |
|-------|-----------|-----------|--------|
| sum-types | `crates/sum-types/src/rpc_types.rs` | 4 | PASS |
| sum-net | `crates/sum-net/src/identity.rs` | 4 (added to existing 5) | PASS |
| sum-store | `crates/sum-store/src/manifest_index.rs` | 6 | PASS |
| sum-node | `crates/sum-node/src/tx_builder.rs` | 2 | PASS |
| **New total** | | **16** | **ALL PASS** |

### Full Test Counts by Crate

| Crate | Phase 1 | Phase 2 | Total | Status |
|-------|---------|---------|-------|--------|
| sum-types | 4 | 4 | 8 | PASS |
| sum-net | 9 | 4 | 13 | PASS |
| sum-store | 34 | 6 | 40 | PASS |
| sum-node | 0 | 2 | 2 | PASS |
| **Total** | **47** | **16** | **63** | **ALL PASS** |

---

## Raw Test Output

```
Running unittests src/lib.rs (sum_net)

running 13 tests
test identity::tests::base58_wrong_length_fails ... ok
test identity::tests::deterministic_keypair ... ok
test identity::tests::base58_bad_checksum_fails ... ok
test codec::tests::request_round_trip ... ok
test codec::tests::error_response_round_trip ... ok
test codec::tests::rejects_oversized_message ... ok
test identity::tests::base58_round_trip ... ok
test identity::tests::different_seeds_different_peerids ... ok
test identity::tests::l1_address_hex_format ... ok
test codec::tests::response_round_trip ... ok
test identity::tests::l1_address_from_peer_pubkey ... ok
test identity::tests::l1_address_is_20_bytes ... ok
test identity::tests::l1_address_derivation ... ok

test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running unittests src/main.rs (sum_node)

running 2 tests
test tx_builder::tests::build_and_verify_proof_tx ... ok
test tx_builder::tests::deterministic_tx_hex ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running unittests src/lib.rs (sum_store)

running 40 tests
test announce::tests::announcement_round_trip ... ok
test chunker::tests::chunk_empty_file ... ok
test content_id::tests::blake3_hex_length ... ok
test content_id::tests::cid_from_hash_matches_cid_from_data ... ok
test content_id::tests::cid_is_deterministic ... ok
test content_id::tests::different_data_different_cid ... ok
test chunker::tests::merkle_root_is_non_zero_for_nonempty_file ... ok
test chunker::tests::chunk_one_byte_file ... ok
test manifest::tests::json_export ... ok
test manifest_index::tests::empty_index ... ok
test manifest_index::tests::chunk_cid_lookup ... ok
test manifest_index::tests::insert_and_lookup_by_root ... ok
test manifest::tests::cbor_round_trip ... ok
test manifest_index::tests::lookup_by_cid ... ok
test manifest_index::tests::legacy_manifest_migration ... ok
test manifest_index::tests::persistence_across_reload ... ok
test merkle::tests::empty_tree ... ok
test merkle::tests::four_leaves_power_of_two ... ok
test merkle::tests::five_leaves_depth_three ... ok
test merkle::tests::single_leaf ... ok
test merkle::tests::three_leaves_odd_duplication ... ok
test merkle::tests::two_leaves ... ok
test merkle::tests::wrong_hash_fails_verification ... ok
test merkle::tests::wrong_index_fails_verification ... ok
test store::tests::get_missing_returns_not_found ... ok
test mmap::tests::mmap_round_trip ... ok
test store::tests::mmap_chunk ... ok
test merkle::tests::cross_validate_all_proofs_with_l1_verifier ... ok
test store::tests::put_get_has_round_trip ... ok
test verify::tests::verify_blake3_bad ... ok
test verify::tests::verify_blake3_good ... ok
test verify::tests::verify_cid_bad ... ok
test verify::tests::verify_cid_good ... ok
test verify::tests::verify_merkle_proof_single_chunk ... ok
test verify::tests::verify_merkle_proof_two_chunks ... ok
test merkle::tests::depth_matches_l1_formula ... ok
test chunker::tests::chunk_exact_1mb ... ok
test chunker::tests::chunk_2_5mb ... ok
test chunker::tests::chunk_5mb_exact ... ok
test chunker::tests::deterministic_manifest ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running unittests src/lib.rs (sum_types)

running 8 tests
test rpc_types::tests::storage_file_info_deserialize ... ok
test storage::tests::chunk_size_matches_l1 ... ok
test rpc_types::tests::storage_file_info_empty_access_list ... ok
test rpc_types::tests::challenge_info_deserialize ... ok
test rpc_types::tests::node_record_info_deserialize ... ok
test config::tests::store_config_defaults ... ok
test storage::tests::chunk_descriptor_serde_round_trip ... ok
test storage::tests::data_manifest_serde_round_trip ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
