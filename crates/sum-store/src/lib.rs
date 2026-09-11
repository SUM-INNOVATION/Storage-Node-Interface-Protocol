pub mod announce;
pub mod assignment;
pub mod assignment_v2;
pub mod chunker;
pub mod content_id;
pub mod error;
pub mod fetch;
pub mod gc;
pub mod lease;
pub mod manifest;
pub mod manifest_index;
pub mod merkle;
pub mod mmap;
mod publication;
pub mod serve;
pub mod store;
pub mod verify;

pub use announce::{ChunkAnnouncement, decode_announcement, encode_announcement};
pub use assignment::{chunks_for_node, compute_chunk_assignment, nodes_for_chunk};
pub use chunker::BinaryChunker;
pub use content_id::cid_from_data;
pub use error::StoreError;
pub use fetch::{FetchManager, FetchNet, FetchOutcome};
pub use lease::{LEASE_FILE_NAME, StoreLease};
pub use manifest_index::ManifestIndex;
pub use merkle::MerkleTree;
pub use store::ChunkStore;

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
    /// Exclusive lease on `config.store_dir`, held for as long as this store
    /// exists. Private: the point of it is that it cannot be released
    /// independently of the store it protects.
    lease: StoreLease,
}

impl SumStore {
    /// Open (or create) the chunk store from the given config.
    ///
    /// The exclusive lease is taken before the chunk store and manifest index
    /// are constructed, and fails if another process already holds the root.
    /// `ChunkStore::mmap`'s safety argument assumes the root has exactly one
    /// manager; this is where that stops being an assumption.
    ///
    /// Precisely: [`StoreLease::acquire`] does `create_dir_all` on the root and
    /// opens `.store.lease` before it calls `flock`, so the root directory and
    /// that one file may exist before the lock is held. Nothing else is written,
    /// and the lease file is opened with `truncate(false)` so its contents are
    /// never modified by a process that has not been granted the root. The
    /// guarantee is therefore "no chunk or manifest state is constructed before
    /// the lock", not "nothing is created before the lock" — an earlier draft of
    /// this comment claimed the latter, which the code does not do.
    pub fn new(config: StoreConfig) -> Result<Self> {
        let lease = StoreLease::acquire(&config.store_dir)?;
        let local = ChunkStore::new(config.store_dir.clone())?;
        let fetcher = FetchManager::new(config.max_chunk_msg_bytes);
        let manifest_idx = ManifestIndex::load(&config.store_dir)?;
        Ok(Self {
            config,
            local,
            fetcher,
            manifest_idx,
            lease,
        })
    }

    /// Path of the exclusive lease file this store holds on its root.
    pub fn lease_path(&self) -> &Path {
        self.lease.path()
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
    pub async fn announce_chunks(&self, net: &SumNet, manifest: &DataManifest) -> Result<()> {
        let merkle_root_hex = manifest
            .merkle_root
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

    /// Health check: returns a snapshot of store health for monitoring.
    pub fn health_check(&self) -> HealthReport {
        let chunk_count = self.local.list_all_cids().map(|v| v.len()).unwrap_or(0);
        let manifest_count = self.manifest_idx.len();
        let disk_usage_bytes = std::fs::read_dir(self.local.root())
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
        let store_dir_writable = {
            let probe = self.config.store_dir.join(".health_probe");
            if std::fs::write(&probe, b"ok").is_ok() {
                let _ = std::fs::remove_file(&probe);
                true
            } else {
                false
            }
        };
        HealthReport {
            chunk_count,
            manifest_count,
            disk_usage_bytes,
            store_dir_writable,
        }
    }
}

/// Health report from [`SumStore::health_check`].
#[derive(Debug)]
pub struct HealthReport {
    pub chunk_count: usize,
    pub manifest_count: usize,
    pub disk_usage_bytes: u64,
    pub store_dir_writable: bool,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::config::StoreConfig;

    #[test]
    fn health_check_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024 * 1024,
        };
        let store = SumStore::new(config).unwrap();

        let report = store.health_check();
        assert_eq!(report.chunk_count, 0);
        assert_eq!(report.manifest_count, 0);
        // disk_usage_bytes sums `metadata().len()` of every direct child
        // of the store root, including subdirectories. Subdirectory
        // sizes are filesystem-dependent — APFS/HFS+ may report ~0 for
        // an empty dir, while ext4/xfs allocate a full block (typically
        // 4096 bytes) per dir. With a few subdirs (`manifests/`, etc.)
        // this can reach tens of KB on Linux for a *truly* empty store.
        // The load-bearing assertion is "no chunks, no manifests, dir
        // writable"; the byte threshold just guards against runaway
        // (e.g. accidentally writing GBs of state on `new()`).
        assert!(
            report.disk_usage_bytes < 64 * 1024,
            "expected minimal disk usage (< 64 KiB), got {}",
            report.disk_usage_bytes
        );
        assert!(report.store_dir_writable);
    }

    #[test]
    fn health_check_with_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024 * 1024,
        };
        let store = SumStore::new(config).unwrap();

        store
            .local
            .put(
                crate::content_id::cid_from_data(b"hello world").as_str(),
                b"hello world",
            )
            .unwrap();
        store
            .local
            .put(
                crate::content_id::cid_from_data(&[0u8; 4096]).as_str(),
                &vec![0u8; 4096],
            )
            .unwrap();

        let report = store.health_check();
        assert_eq!(report.chunk_count, 2);
        assert!(report.disk_usage_bytes > 0);
        assert!(report.store_dir_writable);
    }

    // ── Store-root lease ────────────────────────────────────────────────

    /// Two `SumStore`s cannot share a root.
    ///
    /// Naming a root per node kept honest operators apart; it never stopped
    /// two nodes from being handed the same one. Nothing downstream noticed —
    /// content addressing means concurrent writers produce no write-time
    /// conflict — and the two then garbage-collected against each other's
    /// assignment sets, each deleting chunks the other was obliged to serve.
    #[cfg(unix)]
    #[test]
    fn a_second_store_on_one_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let config = StoreConfig {
            store_dir: store_dir.clone(),
            max_chunk_msg_bytes: 64 * 1024,
        };

        let _held = SumStore::new(config.clone()).expect("first store");
        // `SumStore` is not `Debug`, so `expect_err` is unavailable.
        let err = match SumStore::new(config) {
            Ok(_) => panic!("a second store on one root must be refused"),
            Err(err) => err,
        };

        assert!(
            matches!(err, StoreError::StoreRootBusy { .. }),
            "expected StoreRootBusy, got {err:?}"
        );
    }

    /// The lease is taken before the root is populated, and released with the
    /// store — so a restart re-opens its own root without an unlock step.
    #[cfg(unix)]
    #[test]
    fn a_store_can_reopen_the_root_its_predecessor_released() {
        let dir = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024,
        };

        let first = SumStore::new(config.clone()).expect("first store");
        drop(first);

        SumStore::new(config).expect("restart must not need an unlock step");
    }

    /// The lease file is invisible to every chunk lister, and therefore to GC.
    ///
    /// `list_all_cids` is the sole input to the collector's delete loop and
    /// yields only entries ending in `.chunk`. This runs the collector in its
    /// most destructive configuration — nothing assigned, zero grace — and
    /// asserts it takes the chunk and leaves the lease.
    #[cfg(unix)]
    #[test]
    fn garbage_collection_cannot_sweep_the_lease() {
        use std::collections::HashSet;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let store = SumStore::new(StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024,
        })
        .unwrap();

        let cid = crate::content_id::cid_from_data(b"a sweepable chunk");
        store.local.put(cid.as_str(), b"a sweepable chunk").unwrap();

        assert_eq!(
            store.local.list_all_cids().unwrap(),
            vec![cid.clone()],
            "the lease must not appear as a CID"
        );

        let mut gc = crate::gc::GarbageCollector::new(Duration::from_secs(0));
        let result = gc
            .mark_and_sweep(&store.local, &HashSet::new(), Instant::now())
            .unwrap();

        assert_eq!(result.chunks_deleted, 1, "the chunk was sweepable");
        assert!(!store.local.has(&cid));
        assert!(
            store.lease_path().exists(),
            "GC removed the lease file at {}",
            store.lease_path().display()
        );
    }
}
