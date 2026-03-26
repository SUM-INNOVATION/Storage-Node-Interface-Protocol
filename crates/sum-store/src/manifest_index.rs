//! Persistent manifest index for fast lookup by merkle root or chunk CID.
//!
//! Manifests are stored as individual CBOR files in `<store_dir>/manifests/`.
//! On startup, all manifests are loaded into memory and indexed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use sum_types::storage::DataManifest;

use crate::error::Result;
use crate::manifest;

/// In-memory index of all tracked file manifests.
pub struct ManifestIndex {
    manifests_dir: PathBuf,
    /// Primary index: merkle_root -> manifest.
    by_root: HashMap<[u8; 32], DataManifest>,
    /// Reverse index: chunk CID -> merkle_root.
    cid_to_root: HashMap<String, [u8; 32]>,
}

impl ManifestIndex {
    /// Load all manifests from `<store_dir>/manifests/*.cbor` into memory.
    ///
    /// If `manifests/` is empty but a legacy `manifest.cbor` exists in
    /// `store_dir`, it will be migrated into the new directory.
    pub fn load(store_dir: &Path) -> Result<Self> {
        let manifests_dir = store_dir.join("manifests");
        fs::create_dir_all(&manifests_dir)?;

        let mut index = Self {
            manifests_dir,
            by_root: HashMap::new(),
            cid_to_root: HashMap::new(),
        };

        // Load all existing manifests.
        for entry in fs::read_dir(&index.manifests_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("cbor") {
                match manifest::read_manifest(&path) {
                    Ok(m) => index.add_to_maps(m),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), %e, "skipping corrupt manifest");
                    }
                }
            }
        }

        // Migrate legacy manifest.cbor if the index is empty.
        let legacy_path = store_dir.join("manifest.cbor");
        if index.by_root.is_empty() && legacy_path.exists() {
            if let Ok(m) = manifest::read_manifest(&legacy_path) {
                tracing::info!("migrating legacy manifest.cbor into manifests/");
                // Write to new location (ignore write errors on migration).
                let _ = index.write_manifest(&m);
                index.add_to_maps(m);
            }
        }

        tracing::info!(
            manifests = index.by_root.len(),
            chunks = index.cid_to_root.len(),
            "manifest index loaded"
        );

        Ok(index)
    }

    /// Insert a manifest: write to disk and update in-memory indexes.
    pub fn insert(&mut self, manifest: &DataManifest) -> Result<()> {
        self.write_manifest(manifest)?;
        self.add_to_maps(manifest.clone());
        Ok(())
    }

    /// Look up a manifest by its merkle root.
    pub fn get_by_merkle_root(&self, root: &[u8; 32]) -> Option<&DataManifest> {
        self.by_root.get(root)
    }

    /// Get the CID for a specific chunk within a file.
    pub fn chunk_cid(&self, root: &[u8; 32], chunk_index: u32) -> Option<&str> {
        self.by_root
            .get(root)
            .and_then(|m| m.chunks.get(chunk_index as usize))
            .map(|c| c.cid.as_str())
    }

    /// Reverse lookup: find which file (merkle_root) a chunk CID belongs to.
    pub fn merkle_root_for_cid(&self, cid: &str) -> Option<&[u8; 32]> {
        self.cid_to_root.get(cid)
    }

    /// All tracked merkle roots.
    pub fn all_merkle_roots(&self) -> Vec<[u8; 32]> {
        self.by_root.keys().copied().collect()
    }

    /// Number of tracked manifests.
    pub fn len(&self) -> usize {
        self.by_root.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn add_to_maps(&mut self, manifest: DataManifest) {
        let root = manifest.merkle_root;
        for chunk in &manifest.chunks {
            self.cid_to_root.insert(chunk.cid.clone(), root);
        }
        self.by_root.insert(root, manifest);
    }

    fn write_manifest(&self, manifest: &DataManifest) -> Result<()> {
        let hex_root: String = manifest
            .merkle_root
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = self.manifests_dir.join(format!("{hex_root}.cbor"));
        manifest::write_manifest(manifest, &path)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    fn sample_manifest(root_byte: u8) -> DataManifest {
        DataManifest {
            file_name: format!("test_{root_byte}.bin"),
            file_hash: [root_byte; 32],
            total_size_bytes: 2_097_152,
            chunk_count: 2,
            merkle_root: [root_byte; 32],
            chunks: vec![
                ChunkDescriptor {
                    chunk_index: 0,
                    offset: 0,
                    size: 1_048_576,
                    blake3_hash: [root_byte + 1; 32],
                    cid: format!("bafk_chunk0_{root_byte}"),
                },
                ChunkDescriptor {
                    chunk_index: 1,
                    offset: 1_048_576,
                    size: 1_048_576,
                    blake3_hash: [root_byte + 2; 32],
                    cid: format!("bafk_chunk1_{root_byte}"),
                },
            ],
        }
    }

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

    #[test]
    fn empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ManifestIndex::load(dir.path()).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }
}
