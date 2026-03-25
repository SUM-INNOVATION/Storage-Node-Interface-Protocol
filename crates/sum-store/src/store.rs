//! On-disk chunk storage.
//!
//! Layout: `<chunk_dir>/<cid>.chunk`
//!
//! Files are write-once and content-addressed, so there are no race
//! conditions — if two writers produce the same CID they write identical bytes.

use std::fs;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::error::{Result, StoreError};
use crate::mmap;

/// Filesystem-backed chunk store keyed by CID.
pub struct ChunkStore {
    chunk_dir: PathBuf,
}

impl ChunkStore {
    /// Open (or create) a chunk store rooted at `chunk_dir`.
    pub fn new(chunk_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&chunk_dir)?;
        Ok(Self { chunk_dir })
    }

    /// Path on disk for a given CID.
    pub fn chunk_path(&self, cid: &str) -> PathBuf {
        self.chunk_dir.join(format!("{cid}.chunk"))
    }

    /// Check whether a chunk exists locally.
    pub fn has(&self, cid: &str) -> bool {
        self.chunk_path(cid).exists()
    }

    /// Write chunk data to disk.
    pub fn put(&self, cid: &str, data: &[u8]) -> Result<()> {
        let path = self.chunk_path(cid);
        fs::write(&path, data)?;
        Ok(())
    }

    /// Read entire chunk into memory.
    /// Prefer [`Self::mmap`] for large chunks.
    pub fn get(&self, cid: &str) -> Result<Vec<u8>> {
        let path = self.chunk_path(cid);
        if !path.exists() {
            return Err(StoreError::NotFound(cid.to_string()));
        }
        Ok(fs::read(&path)?)
    }

    /// Memory-map a chunk for zero-copy read access.
    pub fn mmap(&self, cid: &str) -> Result<Mmap> {
        let path = self.chunk_path(cid);
        if !path.exists() {
            return Err(StoreError::NotFound(cid.to_string()));
        }
        mmap::mmap_file(&path)
    }

    /// Write chunk data from an existing memory map to disk.
    pub fn put_from_mmap(&self, cid: &str, data: &Mmap) -> Result<()> {
        self.put(cid, data)
    }

    /// Root directory of this store.
    pub fn root(&self) -> &Path {
        &self.chunk_dir
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn get_missing_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        let err = store.get("nonexistent").unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[test]
    fn mmap_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        let payload = vec![0xABu8; 8192];
        store.put("bafmmap", &payload).unwrap();

        let mapped = store.mmap("bafmmap").unwrap();
        assert_eq!(&*mapped, &payload[..]);
    }
}
