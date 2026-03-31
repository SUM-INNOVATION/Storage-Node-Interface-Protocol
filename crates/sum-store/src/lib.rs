pub mod announce;
pub mod assignment;
pub mod chunker;
pub mod content_id;
pub mod error;
pub mod fetch;
pub mod gc;
pub mod manifest;
pub mod manifest_index;
pub mod merkle;
pub mod mmap;
pub mod serve;
pub mod store;
pub mod verify;

pub use announce::{ChunkAnnouncement, decode_announcement, encode_announcement};
pub use content_id::cid_from_data;
pub use error::StoreError;
pub use fetch::{FetchManager, FetchOutcome};
pub use store::ChunkStore;
pub use chunker::BinaryChunker;
pub use manifest_index::ManifestIndex;
pub use merkle::MerkleTree;
pub use assignment::{compute_chunk_assignment, compute_default_assignment, chunks_for_node, nodes_for_chunk};

use std::path::Path;

use sum_net::{SumNet, TOPIC_STORAGE};
use sum_types::config::StoreConfig;
use sum_types::storage::DataManifest;
use tracing::info;

use crate::error::Result;

/// Top-level API for the SUM Storage Node file storage layer.
pub struct SumStore {
    pub config: StoreConfig,
    pub local: ChunkStore,
    pub fetcher: FetchManager,
    pub manifest_idx: ManifestIndex,
}

impl SumStore {
    /// Open (or create) the chunk store from the given config.
    pub fn new(config: StoreConfig) -> Result<Self> {
        let local = ChunkStore::new(config.store_dir.clone())?;
        let fetcher = FetchManager::new(config.max_chunk_msg_bytes);
        let manifest_idx = ManifestIndex::load(&config.store_dir)?;
        Ok(Self { config, local, fetcher, manifest_idx })
    }

    /// Ingest any file: chunk, compute Merkle tree, store chunks, build manifest.
    pub fn ingest_file(&mut self, path: &Path) -> Result<DataManifest> {
        let (mapped, manifest) = BinaryChunker::chunk_file(path)?;

        info!(
            path = %path.display(),
            chunks = manifest.chunk_count,
            "ingesting file"
        );

        // Write each chunk to disk.
        for chunk in &manifest.chunks {
            if self.local.has(&chunk.cid) {
                info!(cid = %chunk.cid, "chunk already exists — skipping");
                continue;
            }
            let chunk_data = &mapped[chunk.offset as usize..(chunk.offset + chunk.size) as usize];
            self.local.put(&chunk.cid, chunk_data)?;
            info!(
                cid = %chunk.cid,
                index = chunk.chunk_index,
                bytes = chunk_data.len(),
                "chunk written"
            );
        }

        // Write manifest to the index (persistent + in-memory).
        self.manifest_idx.insert(&manifest)?;
        info!(
            merkle_root = %manifest.merkle_root.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "manifest indexed"
        );

        Ok(manifest)
    }

    /// Announce all chunks in a manifest via Gossipsub.
    pub async fn announce_chunks(
        &self,
        net: &SumNet,
        manifest: &DataManifest,
    ) -> Result<()> {
        let merkle_root_hex = manifest.merkle_root
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        for chunk in &manifest.chunks {
            let ann = ChunkAnnouncement {
                merkle_root: merkle_root_hex.clone(),
                chunk_index: chunk.chunk_index,
                chunk_cid: chunk.cid.clone(),
                size_bytes: chunk.size,
            };
            let bytes = encode_announcement(&ann);
            net.publish(TOPIC_STORAGE, bytes)
                .await
                .map_err(|e| StoreError::Other(e.to_string()))?;
            info!(cid = %chunk.cid, index = chunk.chunk_index, "announced chunk");
        }
        Ok(())
    }

    /// Check whether a chunk exists locally.
    pub fn has_chunk(&self, cid: &str) -> bool {
        self.local.has(cid)
    }

    /// Memory-map a local chunk for zero-copy read access.
    pub fn mmap_chunk(&self, cid: &str) -> Result<memmap2::Mmap> {
        self.local.mmap(cid)
    }

    /// Delete all locally stored chunks and manifests.
    ///
    /// Used in client mode after a successful upload — Alice doesn't need
    /// to keep chunks on her disk after they've been pushed to R=3 nodes.
    pub fn cleanup(&self) -> Result<()> {
        let cids = self.local.list_all_cids()?;
        for cid in &cids {
            self.local.delete(cid)?;
        }
        // Also clean up manifests directory.
        let manifests_dir = self.config.store_dir.join("manifests");
        if manifests_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&manifests_dir) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        info!("store cleanup complete — {} chunks removed", cids.len());
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::config::StoreConfig;

    #[test]
    fn cleanup_removes_all_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024 * 1024,
        };
        let store = SumStore::new(config).unwrap();

        // Put some chunks
        store.local.put("cid_a", b"chunk a").unwrap();
        store.local.put("cid_b", b"chunk b").unwrap();
        store.local.put("cid_c", b"chunk c").unwrap();
        assert_eq!(store.local.list_all_cids().unwrap().len(), 3);

        // Cleanup
        store.cleanup().unwrap();

        // Verify empty
        assert_eq!(store.local.list_all_cids().unwrap().len(), 0);
        assert!(!store.local.has("cid_a"));
        assert!(!store.local.has("cid_b"));
        assert!(!store.local.has("cid_c"));
    }
}
