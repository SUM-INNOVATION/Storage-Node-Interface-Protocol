# Phase 1 Test Report — SUM Storage Node Protocol

**Date:** 2026-03-24
**Rust toolchain:** 1.85+ (2024 edition)
**Command:** `cargo test --workspace`
**Result:** 47 passed, 0 failed

---

## Summary

| Crate | Source File | Tests | Status |
|-------|-----------|-------|--------|
| sum-types | `crates/sum-types/src/storage.rs` | 3 | PASS |
| sum-types | `crates/sum-types/src/config.rs` | 1 | PASS |
| sum-net | `crates/sum-net/src/codec.rs` | 4 | PASS |
| sum-net | `crates/sum-net/src/identity.rs` | 5 | PASS |
| sum-store | `crates/sum-store/src/chunker.rs` | 7 | PASS |
| sum-store | `crates/sum-store/src/merkle.rs` | 9 | PASS |
| sum-store | `crates/sum-store/src/verify.rs` | 5 | PASS |
| sum-store | `crates/sum-store/src/content_id.rs` | 4 | PASS |
| sum-store | `crates/sum-store/src/store.rs` | 3 | PASS |
| sum-store | `crates/sum-store/src/manifest.rs` | 2 | PASS |
| sum-store | `crates/sum-store/src/announce.rs` | 1 | PASS |
| sum-store | `crates/sum-store/src/mmap.rs` | 1 | PASS |
| sum-node | `crates/sum-node/src/main.rs` | 0 | N/A |
| **Total** | | **47** | **ALL PASS** |

---

## Raw Test Output

```
Running unittests src/lib.rs (sum_net)

running 9 tests
test identity::tests::l1_address_hex_format ... ok
test identity::tests::different_seeds_different_peerids ... ok
test identity::tests::deterministic_keypair ... ok
test identity::tests::l1_address_is_20_bytes ... ok
test identity::tests::l1_address_derivation ... ok
test codec::tests::rejects_oversized_message ... ok
test codec::tests::request_round_trip ... ok
test codec::tests::error_response_round_trip ... ok
test codec::tests::response_round_trip ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running unittests src/lib.rs (sum_store)

running 34 tests
test chunker::tests::chunk_empty_file ... ok
test announce::tests::announcement_round_trip ... ok
test content_id::tests::blake3_hex_length ... ok
test content_id::tests::cid_is_deterministic ... ok
test content_id::tests::cid_from_hash_matches_cid_from_data ... ok
test chunker::tests::merkle_root_is_non_zero_for_nonempty_file ... ok
test chunker::tests::chunk_one_byte_file ... ok
test content_id::tests::different_data_different_cid ... ok
test manifest::tests::cbor_round_trip ... ok
test manifest::tests::json_export ... ok
test merkle::tests::empty_tree ... ok
test merkle::tests::five_leaves_depth_three ... ok
test merkle::tests::cross_validate_all_proofs_with_l1_verifier ... ok
test merkle::tests::four_leaves_power_of_two ... ok
test merkle::tests::single_leaf ... ok
test merkle::tests::three_leaves_odd_duplication ... ok
test merkle::tests::two_leaves ... ok
test merkle::tests::wrong_hash_fails_verification ... ok
test merkle::tests::wrong_index_fails_verification ... ok
test mmap::tests::mmap_round_trip ... ok
test store::tests::get_missing_returns_not_found ... ok
test store::tests::mmap_chunk ... ok
test merkle::tests::depth_matches_l1_formula ... ok
test verify::tests::verify_blake3_bad ... ok
test verify::tests::verify_blake3_good ... ok
test store::tests::put_get_has_round_trip ... ok
test verify::tests::verify_cid_bad ... ok
test verify::tests::verify_cid_good ... ok
test verify::tests::verify_merkle_proof_single_chunk ... ok
test verify::tests::verify_merkle_proof_two_chunks ... ok
test chunker::tests::chunk_exact_1mb ... ok
test chunker::tests::chunk_2_5mb ... ok
test chunker::tests::chunk_5mb_exact ... ok
test chunker::tests::deterministic_manifest ... ok

test result: ok. 34 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Running unittests src/lib.rs (sum_types)

running 4 tests
test storage::tests::chunk_size_matches_l1 ... ok
test config::tests::store_config_defaults ... ok
test storage::tests::chunk_descriptor_serde_round_trip ... ok
test storage::tests::data_manifest_serde_round_trip ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
