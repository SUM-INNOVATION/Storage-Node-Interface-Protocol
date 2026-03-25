//! BLAKE3 integrity verification and Merkle proof validation.

use crate::content_id;
use crate::error::{Result, StoreError};

/// Verify that `data` matches the expected BLAKE3 hex hash.
pub fn verify_blake3(data: &[u8], expected_hex: &str) -> Result<()> {
    let actual = blake3::hash(data).to_hex().to_string();
    if actual != expected_hex {
        return Err(StoreError::IntegrityMismatch {
            expected: expected_hex.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Verify that `data` hashes to the expected CIDv1 string.
pub fn verify_cid(data: &[u8], expected_cid: &str) -> Result<()> {
    let actual = content_id::cid_from_data(data);
    if actual != expected_cid {
        return Err(StoreError::IntegrityMismatch {
            expected: expected_cid.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Verify a Merkle proof using the same algorithm as the SUM Chain L1.
///
/// This is a direct port of `verify_merkle_proof()` from
/// `sum-chain/crates/state/src/storage_metadata.rs:84-110`.
///
/// - `chunk_hash`: BLAKE3 hash of the chunk data
/// - `chunk_index`: zero-based position in the leaf array
/// - `merkle_path`: sibling hashes, bottom-up
/// - `expected_root`: the file's Merkle root (from L1 `StorageMetadata`)
pub fn verify_merkle_proof(
    chunk_hash: &blake3::Hash,
    chunk_index: u32,
    merkle_path: &[blake3::Hash],
    expected_root: &blake3::Hash,
) -> bool {
    let mut current = *chunk_hash;

    for (level, sibling) in merkle_path.iter().enumerate() {
        let mut hasher = blake3::Hasher::new();

        // Bit at this level determines if we are a left or right child
        if (chunk_index >> level) & 1 == 0 {
            // Left child: H(current || sibling)
            hasher.update(current.as_bytes());
            hasher.update(sibling.as_bytes());
        } else {
            // Right child: H(sibling || current)
            hasher.update(sibling.as_bytes());
            hasher.update(current.as_bytes());
        }

        current = hasher.finalize();
    }

    current == *expected_root
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_blake3_good() {
        let data = b"integrity test data";
        let hex = blake3::hash(data).to_hex().to_string();
        verify_blake3(data, &hex).unwrap();
    }

    #[test]
    fn verify_blake3_bad() {
        let data = b"integrity test data";
        let bad = "0".repeat(64);
        let err = verify_blake3(data, &bad).unwrap_err();
        assert!(matches!(err, StoreError::IntegrityMismatch { .. }));
    }

    #[test]
    fn verify_cid_good() {
        let data = b"cid verification";
        let cid = content_id::cid_from_data(data);
        verify_cid(data, &cid).unwrap();
    }

    #[test]
    fn verify_cid_bad() {
        let data = b"cid verification";
        let err = verify_cid(data, "bafkBADCID").unwrap_err();
        assert!(matches!(err, StoreError::IntegrityMismatch { .. }));
    }

    #[test]
    fn verify_merkle_proof_single_chunk() {
        let h = blake3::hash(b"only chunk");
        // Single chunk: root == chunk_hash, empty proof
        assert!(verify_merkle_proof(&h, 0, &[], &h));
    }

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
}
