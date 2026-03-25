# sum-types — Test Details

**Crate:** `crates/sum-types/`
**Tests:** 4
**Result:** ALL PASS

---

## storage.rs (3 tests)

**Source:** `crates/sum-types/src/storage.rs`

### 1. `chunk_size_matches_l1`

**Purpose:** Confirms the CHUNK_SIZE constant matches the L1 blockchain's constant exactly.

```rust
#[test]
fn chunk_size_matches_l1() {
    assert_eq!(CHUNK_SIZE, 1_048_576);
}
```

**Output:** `ok`
**Why it matters:** If this value drifts from the L1, every Merkle root computed off-chain would mismatch, and all PoR proofs would fail.

---

### 2. `chunk_descriptor_serde_round_trip`

**Purpose:** Verifies that a `ChunkDescriptor` survives JSON serialization and deserialization with all fields intact.

```rust
#[test]
fn chunk_descriptor_serde_round_trip() {
    let cd = ChunkDescriptor {
        chunk_index: 0,
        offset: 0,
        size: CHUNK_SIZE,
        blake3_hash: [0xaa; 32],
        cid: "bafkr4itest".into(),
    };
    let json = serde_json::to_string(&cd).unwrap();
    let round: ChunkDescriptor = serde_json::from_str(&json).unwrap();
    assert_eq!(cd, round);
}
```

**Output:** `ok`
**Why it matters:** `ChunkDescriptor` is serialized to CBOR for manifest storage and JSON for debugging. The `[u8; 32]` hash field must round-trip correctly.

---

### 3. `data_manifest_serde_round_trip`

**Purpose:** Verifies that a `DataManifest` survives JSON serialization and deserialization.

```rust
#[test]
fn data_manifest_serde_round_trip() {
    let m = DataManifest {
        file_name: "test.bin".into(),
        file_hash: [0xbb; 32],
        total_size_bytes: 2_500_000,
        chunk_count: 3,
        merkle_root: [0xcc; 32],
        chunks: vec![],
    };
    let json = serde_json::to_string(&m).unwrap();
    let round: DataManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(round.file_name, "test.bin");
    assert_eq!(round.chunk_count, 3);
}
```

**Output:** `ok`

---

## config.rs (1 test)

**Source:** `crates/sum-types/src/config.rs`

### 4. `store_config_defaults`

**Purpose:** Verifies default `StoreConfig` values are sensible.

```rust
#[test]
fn store_config_defaults() {
    let cfg = StoreConfig::default();
    assert_eq!(cfg.max_chunk_msg_bytes, 2 * 1024 * 1024);
    assert!(cfg.store_dir.ends_with("store"));
}
```

**Output:** `ok`
**Why it matters:** Default store dir should point to `~/.sumnode/store/` and max message size should be 2 MiB.
