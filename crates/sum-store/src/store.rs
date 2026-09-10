//! Immutable filesystem storage under an exclusively managed root.
//!
//! Published files are never truncated or overwritten by this store. External
//! writers and writable aliases remain outside the mapping lifetime guarantee.

use crate::error::{Result, StoreError};
use crate::publication::PublicationStore;
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// Filesystem-backed chunk store. New writes require canonical content CIDs;
/// confined legacy aliases remain readable and deletable.
pub struct ChunkStore {
    files: PublicationStore,
}

impl ChunkStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            files: PublicationStore::new(root, ".chunk")?,
        })
    }

    /// A validated path for interoperability with trusted local tools.
    /// Returning this path does not authorize mutation of a published inode.
    pub fn chunk_path(&self, cid: &str) -> Result<PathBuf> {
        self.files.path(cid)
    }

    /// Convenience presence check. Publication never uses this to skip errors
    /// or to infer content identity/durability.
    pub fn has(&self, cid: &str) -> bool {
        self.files.has(cid)
    }

    pub fn put(&self, cid: &str, data: &[u8]) -> Result<()> {
        self.files.put(cid, data)
    }

    pub fn get(&self, cid: &str) -> Result<Vec<u8>> {
        self.files.get(cid)
    }

    /// Map a regular store-managed file without following a destination symlink.
    /// The root must not have other writers or writable aliases while mapped.
    pub fn mmap(&self, cid: &str) -> Result<Mmap> {
        let file = self
            .files
            .open(cid)?
            .ok_or_else(|| StoreError::NotFound(cid.to_owned()))?;
        // SAFETY: cooperating store APIs never mutate a published inode. This
        // relies on the documented exclusive management of the store root.
        Ok(unsafe { Mmap::map(&file)? })
    }

    pub fn put_from_mmap(&self, cid: &str, data: &Mmap) -> Result<()> {
        self.put(cid, data)
    }

    pub fn root(&self) -> &Path {
        self.files.root()
    }

    pub fn list_all_cids(&self) -> Result<Vec<String>> {
        let mut cids = Vec::new();
        for entry in std::fs::read_dir(self.root())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(key) = name.to_str().and_then(|name| name.strip_suffix(".chunk")) else {
                continue;
            };
            if !entry.file_type()?.is_file() || self.files.path(key).is_err() {
                continue;
            }
            if self.files.open(key)?.is_some() {
                cids.push(key.to_owned());
            }
        }
        Ok(cids)
    }

    pub fn delete(&self, cid: &str) -> Result<bool> {
        self.files.delete(cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_public_path_apis_reject_unconfined_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().to_owned()).unwrap();
        let mapped = memmap2::MmapMut::map_anon(1)
            .unwrap()
            .make_read_only()
            .unwrap();
        for key in ["../escape", "a/b", "a\\b", "", ".", "..", "bad\0key"] {
            assert!(store.chunk_path(key).is_err());
            assert!(!store.has(key));
            assert!(store.get(key).is_err());
            assert!(store.mmap(key).is_err());
            assert!(store.put(key, b"x").is_err());
            assert!(store.put_from_mmap(key, &mapped).is_err());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn put_get_has_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        assert!(!store.has(crate::content_id::cid_from_data(b"chunk payload").as_str()));

        store
            .put(
                crate::content_id::cid_from_data(b"chunk payload").as_str(),
                b"chunk payload",
            )
            .unwrap();
        assert!(store.has(crate::content_id::cid_from_data(b"chunk payload").as_str()));

        let data = store
            .get(crate::content_id::cid_from_data(b"chunk payload").as_str())
            .unwrap();
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
    fn list_all_cids_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();
        let cids = store.list_all_cids().unwrap();
        assert!(cids.is_empty());
    }

    #[test]
    fn list_all_cids_populated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        store
            .put(
                crate::content_id::cid_from_data(b"chunk a").as_str(),
                b"chunk a",
            )
            .unwrap();
        store
            .put(
                crate::content_id::cid_from_data(b"chunk b").as_str(),
                b"chunk b",
            )
            .unwrap();
        store
            .put(
                crate::content_id::cid_from_data(b"chunk c").as_str(),
                b"chunk c",
            )
            .unwrap();

        let mut cids = store.list_all_cids().unwrap();
        cids.sort();
        let mut expected = vec![
            crate::content_id::cid_from_data(b"chunk a"),
            crate::content_id::cid_from_data(b"chunk b"),
            crate::content_id::cid_from_data(b"chunk c"),
        ];
        expected.sort();
        assert_eq!(cids, expected);
    }

    #[test]
    fn delete_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        store
            .put(
                crate::content_id::cid_from_data(b"to be deleted").as_str(),
                b"to be deleted",
            )
            .unwrap();
        assert!(store.has(crate::content_id::cid_from_data(b"to be deleted").as_str()));

        let deleted = store
            .delete(crate::content_id::cid_from_data(b"to be deleted").as_str())
            .unwrap();
        assert!(deleted);
        assert!(!store.has(crate::content_id::cid_from_data(b"to be deleted").as_str()));
    }

    #[test]
    fn delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        let deleted = store.delete("bafnope").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn mmap_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();

        let payload = vec![0xABu8; 8192];
        store
            .put(
                crate::content_id::cid_from_data(&payload).as_str(),
                &payload,
            )
            .unwrap();

        let mapped = store
            .mmap(crate::content_id::cid_from_data(&payload).as_str())
            .unwrap();
        assert_eq!(&*mapped, &payload[..]);
    }
}
