//! Generic binary file chunker for the SUM Storage Node.
//!
//! Replaces the GGUF/tensor-specific chunking logic. Takes any file, maps it
//! into memory via [`crate::mmap`], and slices it into uniform 1 MB chunks.

use std::path::Path;

use memmap2::Mmap;

use sum_types::storage::{CHUNK_SIZE, ChunkDescriptor, DataManifest};

use crate::content_id;
use crate::error::Result;
use crate::merkle::MerkleTree;
use crate::mmap;

/// Chunk any file into uniform 1 MB pieces with BLAKE3 hashing.
pub struct BinaryChunker;

impl BinaryChunker {
    /// Ingest a file: mmap, slice into 1 MB chunks, compute hashes, build
    /// Merkle tree, and return the manifest along with the mmap handle.
    ///
    /// The caller retains ownership of the [`Mmap`] for writing chunks to disk.
    pub fn chunk_file(path: &Path) -> Result<(Mmap, DataManifest)> {
        let mapped = mmap::mmap_file(path)?;
        let file_len = mapped.len() as u64;

        if file_len == 0 {
            return Ok((mapped, DataManifest {
                file_name: file_name_from_path(path),
                file_hash: [0u8; 32],
                total_size_bytes: 0,
                chunk_count: 0,
                merkle_root: [0u8; 32],
                chunks: vec![],
            }));
        }

        let file_hash = *blake3::hash(&mapped).as_bytes();
        let chunk_count = file_len.div_ceil(CHUNK_SIZE) as u32;

        let mut chunks = Vec::with_capacity(chunk_count as usize);
        let mut chunk_hashes = Vec::with_capacity(chunk_count as usize);

        for i in 0..chunk_count {
            let offset = i as u64 * CHUNK_SIZE;
            let end = (offset + CHUNK_SIZE).min(file_len);
            let chunk_data = &mapped[offset as usize..end as usize];
            let hash = blake3::hash(chunk_data);
            let cid = content_id::cid_from_blake3_hash(&hash);

            chunk_hashes.push(hash);
            chunks.push(ChunkDescriptor {
                chunk_index: i,
                offset,
                size: end - offset,
                blake3_hash: *hash.as_bytes(),
                cid,
            });
        }

        let tree = MerkleTree::build(&chunk_hashes);

        let manifest = DataManifest {
            file_name: file_name_from_path(path),
            file_hash,
            total_size_bytes: file_len,
            chunk_count,
            merkle_root: *tree.root().as_bytes(),
            chunks,
        };

        Ok((mapped, manifest))
    }
}

fn file_name_from_path(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_file(data: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(data).unwrap();
        (dir, path)
    }

    #[test]
    fn chunk_empty_file() {
        let (_dir, path) = make_temp_file(b"");
        let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
        assert_eq!(manifest.chunk_count, 0);
        assert!(manifest.chunks.is_empty());
    }

    #[test]
    fn chunk_one_byte_file() {
        let (_dir, path) = make_temp_file(&[0x42]);
        let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
        assert_eq!(manifest.chunk_count, 1);
        assert_eq!(manifest.chunks[0].size, 1);
        assert_eq!(manifest.chunks[0].offset, 0);
        assert_eq!(manifest.total_size_bytes, 1);
    }

    #[test]
    fn chunk_exact_1mb() {
        let data = vec![0xAA; CHUNK_SIZE as usize];
        let (_dir, path) = make_temp_file(&data);
        let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
        assert_eq!(manifest.chunk_count, 1);
        assert_eq!(manifest.chunks[0].size, CHUNK_SIZE);
    }

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

    #[test]
    fn merkle_root_is_non_zero_for_nonempty_file() {
        let data = vec![0xFF; 100];
        let (_dir, path) = make_temp_file(&data);
        let (_, manifest) = BinaryChunker::chunk_file(&path).unwrap();
        assert_ne!(manifest.merkle_root, [0u8; 32]);
    }
}
