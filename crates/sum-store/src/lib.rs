pub mod announce;
pub mod chunker;
pub mod content_id;
pub mod error;
pub mod fetch;
pub mod manifest;
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
pub use merkle::MerkleTree;

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
}

impl SumStore {
    /// Open (or create) the chunk store from the given config.
    pub fn new(config: StoreConfig) -> Result<Self> {
        let local = ChunkStore::new(config.store_dir.clone())?;
        let fetcher = FetchManager::new(config.max_chunk_msg_bytes);
        Ok(Self { config, local, fetcher })
    }

    /// Ingest any file: chunk, compute Merkle tree, store chunks, build manifest.
    pub fn ingest_file(&self, path: &Path) -> Result<DataManifest> {
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

        // Write manifest to disk.
        let manifest_path = self.config.store_dir.join("manifest.cbor");
        manifest::write_manifest(&manifest, &manifest_path)?;
        info!(path = %manifest_path.display(), "manifest written");

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
}
