# sum-store — Test Details

**Crate:** `crates/sum-store/`
**Tests:** 34
**Result:** ALL PASS

---

## chunker.rs (7 tests)

**Source:** `crates/sum-store/src/chunker.rs`

All chunker tests create temporary files on disk, run `BinaryChunker::chunk_file()`, and verify the resulting `DataManifest`.

### 1. `chunk_empty_file`

**Purpose:** A 0-byte file produces an empty manifest with no chunks.

```rust
#[test]
fn chunk_empty_file() {
    let (_dir, path) = make_temp_file(b"");
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    assert_eq!(manifest.chunk_count, 0);
    assert!(manifest.chunks.is_empty());
}
```

**Output:** `ok`

---

### 2. `chunk_one_byte_file`

**Purpose:** A 1-byte file produces exactly 1 chunk of size 1.

```rust
#[test]
fn chunk_one_byte_file() {
    let (_dir, path) = make_temp_file(&[0x42]);
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    assert_eq!(manifest.chunk_count, 1);
    assert_eq!(manifest.chunks[0].size, 1);
    assert_eq!(manifest.chunks[0].offset, 0);
    assert_eq!(manifest.total_size_bytes, 1);
}
```

**Output:** `ok`

---

### 3. `chunk_exact_1mb`

**Purpose:** A file exactly 1,048,576 bytes (1 MB) produces exactly 1 chunk at full size.

```rust
#[test]
fn chunk_exact_1mb() {
    let data = vec![0xAA; CHUNK_SIZE as usize];
    let (_dir, path) = make_temp_file(&data);
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    assert_eq!(manifest.chunk_count, 1);
    assert_eq!(manifest.chunks[0].size, CHUNK_SIZE);
}
```

**Output:** `ok`

---

### 4. `chunk_2_5mb`

**Purpose:** A 2.5 MB file produces 3 chunks: 1MB, 1MB, 0.5MB with correct offsets.

```rust
#[test]
fn chunk_2_5mb() {
    let size = (CHUNK_SIZE as usize) * 2 + (CHUNK_SIZE as usize) / 2;
    let data = vec![0xBB; size];
    let (_dir, path) = make_temp_file(&data);
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();

    assert_eq!(manifest.chunk_count, 3);
    assert_eq!(manifest.total_size_bytes, size as u64);
    assert_eq!(manifest.chunks[0].size, CHUNK_SIZE);
    assert_eq!(manifest.chunks[0].offset, 0);
    assert_eq!(manifest.chunks[1].size, CHUNK_SIZE);
    assert_eq!(manifest.chunks[1].offset, CHUNK_SIZE);
    assert_eq!(manifest.chunks[2].size, CHUNK_SIZE / 2);
    assert_eq!(manifest.chunks[2].offset, CHUNK_SIZE * 2);
}
```

**Output:** `ok`
**Why it matters:** Validates the last-chunk-smaller edge case and offset arithmetic.

---

### 5. `chunk_5mb_exact`

**Purpose:** A 5 MB file produces exactly 5 equal 1 MB chunks.

```rust
#[test]
fn chunk_5mb_exact() {
    let data = vec![0xCC; CHUNK_SIZE as usize * 5];
    let (_dir, path) = make_temp_file(&data);
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    assert_eq!(manifest.chunk_count, 5);
    for chunk in &manifest.chunks {
        assert_eq!(chunk.size, CHUNK_SIZE);
    }
}
```

**Output:** `ok`

---

### 6. `deterministic_manifest`

**Purpose:** The same file data always produces identical hashes, CIDs, and merkle root regardless of file path.

```rust
#[test]
fn deterministic_manifest() {
    let data = vec![0xDD; CHUNK_SIZE as usize * 3 + 500];
    let (_dir1, path1) = make_temp_file(&data);
    let (_dir2, path2) = make_temp_file(&data);

    let (_, m1) = BinaryChunker::chunk_file(&path1).unwrap();
    let (_, m2) = BinaryChunker::chunk_file(&path2).unwrap();

    assert_eq!(m1.file_hash, m2.file_hash);
    assert_eq!(m1.merkle_root, m2.merkle_root);
    assert_eq!(m1.chunk_count, m2.chunk_count);
    for (c1, c2) in m1.chunks.iter().zip(m2.chunks.iter()) {
        assert_eq!(c1.blake3_hash, c2.blake3_hash);
        assert_eq!(c1.cid, c2.cid);
    }
}
```

**Output:** `ok`
**Why it matters:** The merkle_root is the file's identity on the L1. Two nodes ingesting the same file must compute the same root.

---

### 7. `merkle_root_is_non_zero_for_nonempty_file`

**Purpose:** Any non-empty file produces a non-zero merkle root.

```rust
#[test]
fn merkle_root_is_non_zero_for_nonempty_file() {
    let data = vec![0xFF; 100];
    let (_dir, path) = make_temp_file(&data);
    let (_, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    assert_ne!(manifest.merkle_root, [0u8; 32]);
}
```

**Output:** `ok`

---

## merkle.rs (9 tests)

**Source:** `crates/sum-store/src/merkle.rs`

These tests validate L1-compatible Merkle tree construction and proof generation.

### 8. `empty_tree`

**Purpose:** An empty tree has zero-hash root and depth 0.

```rust
#[test]
fn empty_tree() {
    let tree = MerkleTree::build(&[]);
    assert_eq!(tree.root(), blake3::Hash::from([0u8; 32]));
    assert_eq!(tree.depth(), 0);
    assert_eq!(tree.leaf_count(), 0);
}
```

**Output:** `ok`

---

### 9. `single_leaf`

**Purpose:** A 1-leaf tree: root equals the leaf hash, empty proof, depth 0.

```rust
#[test]
fn single_leaf() {
    let h = make_leaf(b"only chunk");
    let tree = MerkleTree::build(&[h]);
    assert_eq!(tree.root(), h);
    assert_eq!(tree.depth(), 0);
    assert!(tree.generate_proof(0).is_empty());
}
```

**Output:** `ok`
**Why it matters:** Single-chunk files have merkle_root == chunk_hash on the L1.

---

### 10. `two_leaves`

**Purpose:** Manually verifies a 2-leaf tree: root = H(h0 || h1), proofs are the sibling.

```rust
#[test]
fn two_leaves() {
    let h0 = make_leaf(b"chunk 0");
    let h1 = make_leaf(b"chunk 1");
    let tree = MerkleTree::build(&[h0, h1]);

    assert_eq!(tree.depth(), 1);
    assert_eq!(tree.root(), hash_pair(&h0, &h1));

    let proof0 = tree.generate_proof(0);
    assert_eq!(proof0.len(), 1);
    assert_eq!(proof0[0], h1);

    let proof1 = tree.generate_proof(1);
    assert_eq!(proof1.len(), 1);
    assert_eq!(proof1[0], h0);
}
```

**Output:** `ok`

---

### 11. `three_leaves_odd_duplication`

**Purpose:** Verifies the L1's odd-level behavior: the last node is **duplicated** (not promoted).

```rust
#[test]
fn three_leaves_odd_duplication() {
    let h0 = make_leaf(b"chunk 0");
    let h1 = make_leaf(b"chunk 1");
    let h2 = make_leaf(b"chunk 2");
    let tree = MerkleTree::build(&[h0, h1, h2]);

    assert_eq!(tree.depth(), 2);

    // Level 1: [H(h0,h1), H(h2,h2)]  — h2 duplicated per L1 rule
    let l1_0 = hash_pair(&h0, &h1);
    let l1_1 = hash_pair(&h2, &h2);
    let expected_root = hash_pair(&l1_0, &l1_1);
    assert_eq!(tree.root(), expected_root);

    let proof0 = tree.generate_proof(0);
    assert_eq!(proof0, vec![h1, l1_1]);

    let proof2 = tree.generate_proof(2);
    assert_eq!(proof2, vec![h2, l1_0]); // sibling is itself (duplicated)
}
```

**Output:** `ok`
**Why it matters:** This is the most critical L1 alignment test. The L1 at `hash.rs:98` uses `unwrap_or(left)` which duplicates. If we promoted instead, the root would differ and all proofs would fail on-chain.

---

### 12. `four_leaves_power_of_two`

**Purpose:** Power-of-2 tree has expected depth and leaf count (no duplication needed).

```rust
#[test]
fn four_leaves_power_of_two() {
    let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
    let tree = MerkleTree::build(&leaves);
    assert_eq!(tree.depth(), 2);
    assert_eq!(tree.leaf_count(), 4);
}
```

**Output:** `ok`

---

### 13. `five_leaves_depth_three`

**Purpose:** 5 leaves (odd, not power of 2) produces depth 3.

```rust
#[test]
fn five_leaves_depth_three() {
    let leaves: Vec<blake3::Hash> = (0..5u8).map(|i| make_leaf(&[i])).collect();
    let tree = MerkleTree::build(&leaves);
    assert_eq!(tree.depth(), 3);
}
```

**Output:** `ok`

---

### 14. `cross_validate_all_proofs_with_l1_verifier`

**Purpose:** For every tree size from 1 to 17, builds a tree, generates a proof for EVERY chunk, and runs it through the L1 verification algorithm. All must pass.

```rust
#[test]
fn cross_validate_all_proofs_with_l1_verifier() {
    for n in 1..=17u32 {
        let leaves: Vec<blake3::Hash> =
            (0..n).map(|i| make_leaf(&i.to_le_bytes())).collect();
        let tree = MerkleTree::build(&leaves);
        let root = tree.root();

        for idx in 0..n {
            let proof = tree.generate_proof(idx);
            assert!(
                verify_merkle_proof(&leaves[idx as usize], idx, &proof, &root),
                "proof failed: n={n}, idx={idx}"
            );
        }
    }
}
```

**Output:** `ok`
**Why it matters:** This is the comprehensive correctness test. It generates 153 individual proofs (1+2+3+...+17) and verifies every single one against the exact algorithm the L1 uses. Covers all edge cases: single leaf, even counts, odd counts with duplication, various depths.

---

### 15. `wrong_index_fails_verification`

**Purpose:** A proof generated for chunk 0, when claimed as chunk 1, must fail.

```rust
#[test]
fn wrong_index_fails_verification() {
    let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
    let tree = MerkleTree::build(&leaves);
    let root = tree.root();

    let proof0 = tree.generate_proof(0);
    assert!(!verify_merkle_proof(&leaves[0], 1, &proof0, &root));
}
```

**Output:** `ok`
**Why it matters:** Prevents a malicious node from reusing proofs for different chunks.

---

### 16. `wrong_hash_fails_verification`

**Purpose:** A fake chunk hash with a valid proof must fail verification.

```rust
#[test]
fn wrong_hash_fails_verification() {
    let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
    let tree = MerkleTree::build(&leaves);
    let root = tree.root();

    let proof0 = tree.generate_proof(0);
    let fake_hash = make_leaf(b"fake");
    assert!(!verify_merkle_proof(&fake_hash, 0, &proof0, &root));
}
```

**Output:** `ok`
**Why it matters:** Prevents a node from submitting a proof for data it doesn't actually hold.

---

### 17. `depth_matches_l1_formula`

**Purpose:** For tree sizes 2 through 64, verifies the depth matches the L1's formula: `64 - ((N-1) as u64).leading_zeros()`.

```rust
#[test]
fn depth_matches_l1_formula() {
    for n in 2..=64u32 {
        let leaves: Vec<blake3::Hash> =
            (0..n).map(|i| make_leaf(&i.to_le_bytes())).collect();
        let tree = MerkleTree::build(&leaves);
        let expected = 64 - ((n as u64) - 1).leading_zeros() as usize;
        assert_eq!(
            tree.depth(), expected,
            "depth mismatch for n={n}: got {}, expected {expected}", tree.depth()
        );
    }
}
```

**Output:** `ok`
**Why it matters:** The L1 uses this formula to validate proof length. If our depth calculation differs, proof lengths won't match and the L1 will reject them.

---

## verify.rs (5 tests)

**Source:** `crates/sum-store/src/verify.rs`

### 18. `verify_blake3_good`

**Purpose:** Correct BLAKE3 hex hash passes verification.

```rust
#[test]
fn verify_blake3_good() {
    let data = b"integrity test data";
    let hex = blake3::hash(data).to_hex().to_string();
    verify_blake3(data, &hex).unwrap();
}
```

**Output:** `ok`

---

### 19. `verify_blake3_bad`

**Purpose:** Wrong hash produces `IntegrityMismatch` error.

```rust
#[test]
fn verify_blake3_bad() {
    let data = b"integrity test data";
    let bad = "0".repeat(64);
    let err = verify_blake3(data, &bad).unwrap_err();
    assert!(matches!(err, StoreError::IntegrityMismatch { .. }));
}
```

**Output:** `ok`

---

### 20. `verify_cid_good`

**Purpose:** Correct CID passes verification.

```rust
#[test]
fn verify_cid_good() {
    let data = b"cid verification";
    let cid = content_id::cid_from_data(data);
    verify_cid(data, &cid).unwrap();
}
```

**Output:** `ok`

---

### 21. `verify_cid_bad`

**Purpose:** Wrong CID produces `IntegrityMismatch` error.

```rust
#[test]
fn verify_cid_bad() {
    let data = b"cid verification";
    let err = verify_cid(data, "bafkBADCID").unwrap_err();
    assert!(matches!(err, StoreError::IntegrityMismatch { .. }));
}
```

**Output:** `ok`

---

### 22. `verify_merkle_proof_single_chunk`

**Purpose:** Single-chunk proof: root equals chunk hash, empty proof path passes.

```rust
#[test]
fn verify_merkle_proof_single_chunk() {
    let h = blake3::hash(b"only chunk");
    assert!(verify_merkle_proof(&h, 0, &[], &h));
}
```

**Output:** `ok`

---

### 23. `verify_merkle_proof_two_chunks`

**Purpose:** Manually constructs a 2-chunk tree and verifies both proofs using the L1 algorithm.

```rust
#[test]
fn verify_merkle_proof_two_chunks() {
    let h0 = blake3::hash(b"chunk 0");
    let h1 = blake3::hash(b"chunk 1");
    let mut hasher = blake3::Hasher::new();
    hasher.update(h0.as_bytes());
    hasher.update(h1.as_bytes());
    let root = hasher.finalize();

    assert!(verify_merkle_proof(&h0, 0, &[h1], &root));
    assert!(verify_merkle_proof(&h1, 1, &[h0], &root));
}
```

**Output:** `ok`

---

## content_id.rs (4 tests)

**Source:** `crates/sum-store/src/content_id.rs`

### 24. `cid_is_deterministic`

**Purpose:** Same data always produces the same CID, and it starts with 'b' (base32lower).

```rust
#[test]
fn cid_is_deterministic() {
    let data = b"hello sum-store";
    let a = cid_from_data(data);
    let b = cid_from_data(data);
    assert_eq!(a, b);
    assert!(a.starts_with('b'), "CID should start with 'b': {a}");
}
```

**Output:** `ok`

---

### 25. `different_data_different_cid`

**Purpose:** Different data produces different CIDs.

```rust
#[test]
fn different_data_different_cid() {
    let a = cid_from_data(b"aaa");
    let b = cid_from_data(b"bbb");
    assert_ne!(a, b);
}
```

**Output:** `ok`

---

### 26. `blake3_hex_length`

**Purpose:** BLAKE3 hex string is always 64 characters (32 bytes * 2).

```rust
#[test]
fn blake3_hex_length() {
    let hex = blake3_hex(b"test");
    assert_eq!(hex.len(), 64);
}
```

**Output:** `ok`

---

### 27. `cid_from_hash_matches_cid_from_data`

**Purpose:** Computing CID from data vs. from a pre-computed BLAKE3 hash yields the same result.

```rust
#[test]
fn cid_from_hash_matches_cid_from_data() {
    let data = b"round-trip check";
    let hash = blake3::hash(data);
    assert_eq!(cid_from_data(data), cid_from_blake3_hash(&hash));
}
```

**Output:** `ok`

---

## store.rs (3 tests)

**Source:** `crates/sum-store/src/store.rs`

### 28. `put_get_has_round_trip`

**Purpose:** Write a chunk, confirm it exists, read it back, verify contents match.

```rust
#[test]
fn put_get_has_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

    assert!(!store.has("baftest"));
    store.put("baftest", b"chunk payload").unwrap();
    assert!(store.has("baftest"));

    let data = store.get("baftest").unwrap();
    assert_eq!(data, b"chunk payload");
}
```

**Output:** `ok`

---

### 29. `get_missing_returns_not_found`

**Purpose:** Requesting a non-existent CID returns `StoreError::NotFound`.

```rust
#[test]
fn get_missing_returns_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::new(dir.path().join("chunks")).unwrap();
    let err = store.get("nonexistent").unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}
```

**Output:** `ok`

---

### 30. `mmap_chunk`

**Purpose:** Write a chunk, memory-map it, verify contents match.

```rust
#[test]
fn mmap_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

    let payload = vec![0xABu8; 8192];
    store.put("bafmmap", &payload).unwrap();

    let mapped = store.mmap("bafmmap").unwrap();
    assert_eq!(&*mapped, &payload[..]);
}
```

**Output:** `ok`

---

## manifest.rs (2 tests)

**Source:** `crates/sum-store/src/manifest.rs`

### 31. `cbor_round_trip`

**Purpose:** Write a `DataManifest` as CBOR to disk, read it back, verify all fields match.

```rust
#[test]
fn cbor_round_trip() {
    let manifest = sample_manifest();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.cbor");

    write_manifest(&manifest, &path).unwrap();
    let loaded = read_manifest(&path).unwrap();

    assert_eq!(loaded.file_name, "test-file.bin");
    assert_eq!(loaded.chunk_count, 4);
    assert_eq!(loaded.merkle_root, [0xbb; 32]);
}
```

**Output:** `ok`

---

### 32. `json_export`

**Purpose:** Pretty-printed JSON contains expected field names and values.

```rust
#[test]
fn json_export() {
    let manifest = sample_manifest();
    let json = manifest_to_json(&manifest).unwrap();
    assert!(json.contains("\"file_name\": \"test-file.bin\""));
    assert!(json.contains("\"chunk_count\": 4"));
}
```

**Output:** `ok`

---

## announce.rs (1 test)

**Source:** `crates/sum-store/src/announce.rs`

### 33. `announcement_round_trip`

**Purpose:** A `ChunkAnnouncement` survives bincode encode/decode (used for gossipsub).

```rust
#[test]
fn announcement_round_trip() {
    let ann = ChunkAnnouncement {
        merkle_root: "a".repeat(64),
        chunk_index: 0,
        chunk_cid: "bafkr4itest".into(),
        size_bytes: 1_048_576,
    };
    let bytes = encode_announcement(&ann);
    let decoded = decode_announcement(&bytes).unwrap();
    assert_eq!(decoded.merkle_root, ann.merkle_root);
    assert_eq!(decoded.chunk_index, ann.chunk_index);
    assert_eq!(decoded.chunk_cid, ann.chunk_cid);
    assert_eq!(decoded.size_bytes, ann.size_bytes);
}
```

**Output:** `ok`

---

## mmap.rs (1 test)

**Source:** `crates/sum-store/src/mmap.rs`

### 34. `mmap_round_trip`

**Purpose:** Write a file to disk, memory-map it, verify contents match.

```rust
#[test]
fn mmap_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.bin");

    let data = b"hello mmap";
    { let mut f = File::create(&path).unwrap(); f.write_all(data).unwrap(); }

    let mapped = mmap_file(&path).unwrap();
    assert_eq!(&*mapped, data);
}
```

**Output:** `ok`
