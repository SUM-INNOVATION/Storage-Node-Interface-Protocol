# Phase 2: sum-store — New Test Details

**Crate:** `crates/sum-store/`
**New tests in Phase 2:** 6 (in `manifest_index.rs`, added to existing 34 from Phase 1, total 40)
**Result:** ALL PASS

---

## manifest_index.rs (6 tests)

**Source:** `crates/sum-store/src/manifest_index.rs`

All tests use a shared `sample_manifest(root_byte)` helper that creates a `DataManifest` with 2 chunks, parameterized by a root byte for uniqueness:

```rust
fn sample_manifest(root_byte: u8) -> DataManifest {
    DataManifest {
        file_name: format!("test_{root_byte}.bin"),
        file_hash: [root_byte; 32],
        total_size_bytes: 2_097_152,
        chunk_count: 2,
        merkle_root: [root_byte; 32],
        chunks: vec![
            ChunkDescriptor {
                chunk_index: 0, offset: 0, size: 1_048_576,
                blake3_hash: [root_byte + 1; 32],
                cid: format!("bafk_chunk0_{root_byte}"),
            },
            ChunkDescriptor {
                chunk_index: 1, offset: 1_048_576, size: 1_048_576,
                blake3_hash: [root_byte + 2; 32],
                cid: format!("bafk_chunk1_{root_byte}"),
            },
        ],
    }
}
```

---

### 1. `insert_and_lookup_by_root`

**Purpose:** Insert a manifest, then look it up by merkle_root. Verify the returned manifest has the correct file_name and chunk_count.

```rust
#[test]
fn insert_and_lookup_by_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = ManifestIndex::load(dir.path()).unwrap();

    let m = sample_manifest(0xAA);
    idx.insert(&m).unwrap();

    let found = idx.get_by_merkle_root(&[0xAA; 32]).unwrap();
    assert_eq!(found.file_name, "test_170.bin");
    assert_eq!(found.chunk_count, 2);
}
```

**Output:** `ok`
**Why it matters:** This is the primary lookup used by the PoR worker. When challenged for file `merkle_root=X`, it calls `get_by_merkle_root(X)` to find the manifest and locate chunks.

---

### 2. `lookup_by_cid`

**Purpose:** After inserting a manifest, look up which file a chunk CID belongs to (reverse index).

```rust
#[test]
fn lookup_by_cid() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = ManifestIndex::load(dir.path()).unwrap();

    let m = sample_manifest(0xBB);
    idx.insert(&m).unwrap();

    let root = idx.merkle_root_for_cid("bafk_chunk0_187").unwrap();
    assert_eq!(*root, [0xBB; 32]);

    let root1 = idx.merkle_root_for_cid("bafk_chunk1_187").unwrap();
    assert_eq!(*root1, [0xBB; 32]);

    assert!(idx.merkle_root_for_cid("nonexistent").is_none());
}
```

**Output:** `ok`
**Why it matters:** The ACL interceptor uses this reverse lookup. When a peer requests a chunk by CID, the ACL checker needs to find the file's merkle_root to query the L1 for the access list. Both chunk CIDs must map back to the same merkle_root.

---

### 3. `chunk_cid_lookup`

**Purpose:** Look up the CID for a specific chunk index within a file.

```rust
#[test]
fn chunk_cid_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = ManifestIndex::load(dir.path()).unwrap();

    let m = sample_manifest(0xCC);
    idx.insert(&m).unwrap();

    assert_eq!(idx.chunk_cid(&[0xCC; 32], 0), Some("bafk_chunk0_204"));
    assert_eq!(idx.chunk_cid(&[0xCC; 32], 1), Some("bafk_chunk1_204"));
    assert_eq!(idx.chunk_cid(&[0xCC; 32], 2), None);
}
```

**Output:** `ok`
**Why it matters:** The PoR worker uses this to find the on-disk CID for the challenged chunk. The L1 challenge says "prove chunk 5 of file X" — we need the CID to read the chunk from the `ChunkStore`. Out-of-bounds index correctly returns `None`.

---

### 4. `persistence_across_reload`

**Purpose:** Insert manifests, drop the index, reload from disk, verify all data survives.

```rust
#[test]
fn persistence_across_reload() {
    let dir = tempfile::tempdir().unwrap();

    // Insert two manifests.
    {
        let mut idx = ManifestIndex::load(dir.path()).unwrap();
        idx.insert(&sample_manifest(0x11)).unwrap();
        idx.insert(&sample_manifest(0x22)).unwrap();
        assert_eq!(idx.len(), 2);
    }

    // Reload from disk.
    {
        let idx = ManifestIndex::load(dir.path()).unwrap();
        assert_eq!(idx.len(), 2);
        assert!(idx.get_by_merkle_root(&[0x11; 32]).is_some());
        assert!(idx.get_by_merkle_root(&[0x22; 32]).is_some());
        assert!(idx.merkle_root_for_cid("bafk_chunk0_17").is_some());
    }
}
```

**Output:** `ok`
**Why it matters:** The node must survive restarts without losing track of ingested files. Manifests are written as `<store_dir>/manifests/<hex_root>.cbor`. On reload, all CBOR files are read and both indexes (by_root and cid_to_root) are rebuilt.

---

### 5. `legacy_manifest_migration`

**Purpose:** If a `manifest.cbor` exists in the store root (Phase 1's old format) but the `manifests/` directory is empty, the legacy file is automatically migrated into the new index.

```rust
#[test]
fn legacy_manifest_migration() {
    let dir = tempfile::tempdir().unwrap();
    let m = sample_manifest(0xDD);

    // Write a legacy manifest.cbor in the store root.
    manifest::write_manifest(&m, &dir.path().join("manifest.cbor")).unwrap();

    // Load should migrate it.
    let idx = ManifestIndex::load(dir.path()).unwrap();
    assert_eq!(idx.len(), 1);
    assert!(idx.get_by_merkle_root(&[0xDD; 32]).is_some());
}
```

**Output:** `ok`
**Why it matters:** Phase 1 wrote manifests to a fixed `manifest.cbor` path. Phase 2 uses a per-file directory (`manifests/<root>.cbor`). This migration ensures nodes that ingested files during Phase 1 don't lose their manifests when upgrading.

---

### 6. `empty_index`

**Purpose:** A fresh store directory with no manifests produces an empty index.

```rust
#[test]
fn empty_index() {
    let dir = tempfile::tempdir().unwrap();
    let idx = ManifestIndex::load(dir.path()).unwrap();
    assert!(idx.is_empty());
    assert_eq!(idx.len(), 0);
}
```

**Output:** `ok`
**Why it matters:** Baseline sanity check — a new node with no ingested files should have an empty manifest index, and `is_empty()` / `len()` must agree.
