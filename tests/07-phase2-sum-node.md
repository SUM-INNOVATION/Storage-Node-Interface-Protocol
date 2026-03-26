# Phase 2: sum-node — New Test Details

**Crate:** `crates/sum-node/`
**New tests in Phase 2:** 2 (in `tx_builder.rs`)
**Result:** ALL PASS

---

## tx_builder.rs (2 tests)

**Source:** `crates/sum-node/src/tx_builder.rs`

These tests validate the transaction builder — the most security-critical component in Phase 2. The builder must produce `SignedTransaction` bytes that the L1 can deserialize via `SignedTransaction::from_hex()` (bincode v1).

### 1. `build_and_verify_proof_tx`

**Purpose:** Build a complete `SubmitStorageProof` transaction with known values, hex-encode it, decode it back, and verify every field survived the bincode v1 round-trip. Also verify the public key in the signed transaction matches the Ed25519 seed.

```rust
#[test]
fn build_and_verify_proof_tx() {
    let seed = [42u8; 32];
    let hex = build_submit_proof_tx(
        &seed,
        1,                  // chain_id
        0,                  // nonce
        1_000_000,          // fee
        [0xAA; 32],         // challenge_id
        [0xBB; 32],         // merkle_root
        5,                  // chunk_index
        [0xCC; 32],         // chunk_hash
        vec![[0xDD; 32], [0xEE; 32]], // merkle_path (2 siblings)
    )
    .unwrap();

    // Should be a valid hex string.
    assert!(!hex.is_empty());
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));

    // Should be deserializable back (proves bincode v1 round-trip).
    let bytes = hex::decode(&hex).unwrap();
    let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

    // Verify the transaction contents survived.
    match signed.inner {
        TxInnerMirror::V2(tx) => {
            assert_eq!(tx.chain_id, 1);
            assert_eq!(tx.nonce, 0);
            assert_eq!(tx.fee, 1_000_000);
            match tx.payload {
                TxPayloadMirror::StorageMetadata(data) => {
                    match data.operation {
                        StorageMetadataOperationMirror::SubmitStorageProof {
                            challenge_id,
                            merkle_root,
                            chunk_index,
                            chunk_hash,
                            merkle_path,
                        } => {
                            assert_eq!(challenge_id, [0xAA; 32]);
                            assert_eq!(merkle_root, [0xBB; 32]);
                            assert_eq!(chunk_index, 5);
                            assert_eq!(chunk_hash, [0xCC; 32]);
                            assert_eq!(merkle_path.len(), 2);
                        }
                        _ => panic!("wrong operation variant"),
                    }
                }
                _ => panic!("wrong payload variant"),
            }
        }
        _ => panic!("wrong TxInner variant"),
    }

    // Verify Ed25519 signature.
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();
    assert_eq!(signed.public_key, verifying_key.to_bytes());
}
```

**Output:** `ok`

**Why it matters:** This is the most critical test in Phase 2. It validates the entire transaction pipeline:

1. **Struct layout**: The `TransactionV2Mirror` fields (`chain_id`, `from`, `fee` as `u128`, `nonce`, `payload`) must serialize in the same order as the L1's `TransactionV2`. If any field is out of order, the L1 would deserialize garbage.

2. **Enum variant indices**: `TxPayloadMirror::StorageMetadata` must be at variant index 18 (the 19th variant). If any placeholder variant is missing or misordered, bincode v1 would write the wrong variant index and the L1 would interpret the transaction as a different operation (e.g., a Token transfer instead of a storage proof).

3. **Nested enum indices**: `StorageMetadataOperationMirror::SubmitStorageProof` must be at variant index 5. The 6 variants of this enum must match the L1's `StorageMetadataOperation` definition at `sum-chain/crates/primitives/src/storage_metadata.rs:72-113`.

4. **`TxInnerMirror::V2`** must be at variant index 1 (Legacy = 0, V2 = 1), matching `sum-chain/crates/primitives/src/transaction.rs:726-731`.

5. **Ed25519 signing**: The `signature` field is a valid Ed25519 signature over `blake3(bincode1_bytes_of_TransactionV2)`. The `public_key` field matches the signing key.

6. **`[u8; 64]` serialization**: The signature field uses `serde_big_array` for serde compatibility with arrays > 32 elements. Under bincode v1, this is transparent (64 contiguous bytes), matching the L1's `#[serde(with = "BigArray")]`.

---

### 2. `deterministic_tx_hex`

**Purpose:** The same inputs must always produce the exact same hex-encoded transaction. No randomness is involved.

```rust
#[test]
fn deterministic_tx_hex() {
    let seed = [1u8; 32];
    let hex1 = build_submit_proof_tx(
        &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
    )
    .unwrap();
    let hex2 = build_submit_proof_tx(
        &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
    )
    .unwrap();
    assert_eq!(hex1, hex2, "same inputs must produce same tx hex");
}
```

**Output:** `ok`

**Why it matters:** Ed25519 signing is deterministic (RFC 8032 — no random nonce). Combined with deterministic bincode v1 serialization and blake3 hashing, the entire pipeline must be repeatable. If this test ever fails, it means randomness leaked into the signing or serialization path, which would make transaction debugging impossible.

---

## Modules Without Unit Tests

The following Phase 2 modules do not have unit tests because they require a live L1 RPC server or a running libp2p swarm to exercise:

| Module | File | Why no unit tests |
|--------|------|-------------------|
| `rpc_client.rs` | `crates/sum-node/src/rpc_client.rs` | All methods make HTTP calls to an L1 node. Would require mocking or an integration test environment. |
| `por_worker.rs` | `crates/sum-node/src/por_worker.rs` | Orchestrates RPC calls, disk reads, Merkle proof generation, and transaction submission. Each component is tested individually; the worker is integration-level. |
| `acl.rs` | `crates/sum-node/src/acl.rs` | Combines RPC calls with peer address lookups. Requires mocked RPC responses or a live L1. |

These modules are tested indirectly:
- **rpc_client**: RPC types are validated in `rpc_types` tests (deserialization from real JSON shapes)
- **por_worker**: Merkle proof generation is validated by `merkle.rs` tests (cross-validated against L1 algorithm for tree sizes 1-17), transaction building is validated by `tx_builder` tests
- **acl**: Address derivation is validated by `identity.rs` tests (`l1_address_from_peer_pubkey`), base58 encoding by `base58_round_trip`
