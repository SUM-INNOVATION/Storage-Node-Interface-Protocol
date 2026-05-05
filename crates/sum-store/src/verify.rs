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

/// Wire-format variant of [`verify_merkle_proof`] that takes `[u8; 32]`
/// arrays directly. **Does NOT enforce tree-shape bounds** — accepts
/// any `chunk_index` and any `merkle_path` length and walks the path
/// blindly. Useful only when the caller has already verified bounds.
///
/// **V2 push validation MUST use [`verify_merkle_proof_bytes_for_tree`]
/// instead.** That variant takes the file's `total_leaves` (= chunk
/// count) and rejects out-of-range indices and wrong-length proofs
/// upfront. Without that bound check, a peer can construct a proof
/// against a bogus shape that happens to hash to the chain-state root
/// and inject a chunk into our `cid → root` index for an index the
/// real file doesn't have.
pub fn verify_merkle_proof_bytes(
    chunk_hash: &[u8; 32],
    chunk_index: u32,
    merkle_path: &[[u8; 32]],
    expected_root: &[u8; 32],
) -> bool {
    let chunk_hash = blake3::Hash::from(*chunk_hash);
    let path: Vec<blake3::Hash> = merkle_path.iter().map(|b| blake3::Hash::from(*b)).collect();
    let expected_root = blake3::Hash::from(*expected_root);
    verify_merkle_proof(&chunk_hash, chunk_index, &path, &expected_root)
}

/// Strict V2 push-validation Merkle verifier.
///
/// Rejects upfront:
/// 1. **Empty tree** — `total_leaves == 0` has no valid proofs.
/// 2. **Index out of range** — `chunk_index >= total_leaves`.
/// 3. **Wrong proof length** — `merkle_path.len()` must equal the
///    canonical tree depth `ceil(log2(total_leaves))` for `total_leaves
///    ≥ 2`, or `0` for a single-leaf tree.
///
/// Only after those bounds pass do we delegate to
/// [`verify_merkle_proof_bytes`] for the actual hash chain.
///
/// `total_leaves` is the file's registered `chunk_count` (the
/// `RegisterFilePendingV2` field, replicated into `StorageMetadataV2`).
/// Push validators MUST source it from chain state (via the cached
/// `storage_getFileInfoV2` lookup), not from the push message itself —
/// otherwise a peer could lie about both `chunk_count` and `chunk_index`
/// to bypass the bounds check.
pub fn verify_merkle_proof_bytes_for_tree(
    chunk_hash: &[u8; 32],
    chunk_index: u32,
    merkle_path: &[[u8; 32]],
    expected_root: &[u8; 32],
    total_leaves: u32,
) -> bool {
    if total_leaves == 0 {
        return false;
    }
    if chunk_index >= total_leaves {
        return false;
    }
    let expected_depth = expected_proof_depth(total_leaves);
    if merkle_path.len() != expected_depth {
        return false;
    }
    if total_leaves == 1 {
        // Empty proof is mandatory (covered by depth check above) and
        // root must equal the leaf hash.
        return chunk_hash == expected_root;
    }
    verify_merkle_proof_bytes(chunk_hash, chunk_index, merkle_path, expected_root)
}

/// Canonical Merkle proof depth for a tree of `total_leaves` leaves.
///
/// Matches `MerkleTree::depth()`:
/// * `0 leaves` → `0` (no tree; caller rejects this case before calling)
/// * `1 leaf`   → `0` (root is the leaf; empty proof)
/// * `n ≥ 2`    → `ceil(log2(n))` (number of `(n-1).leading_zeros()` bits)
///
/// Computed branch-free for `n ≥ 2` via `32 - (n-1).leading_zeros()`.
fn expected_proof_depth(total_leaves: u32) -> usize {
    match total_leaves {
        0 | 1 => 0,
        n => (u32::BITS - (n - 1).leading_zeros()) as usize,
    }
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

    /// W2 wire-format API: end-to-end roundtrip across tree shapes that
    /// exercise the odd-leaf-count duplication codepath (size 3, 5).
    /// This is the V2 push validator's exact use case — it gets `[u8; 32]`
    /// values off the wire and must verify against the chain-state root.
    #[test]
    fn verify_merkle_proof_bytes_roundtrip_across_sizes() {
        use crate::merkle::MerkleTree;

        for size in [1usize, 2, 3, 4, 5, 8, 17, 64] {
            let leaves: Vec<blake3::Hash> = (0..size)
                .map(|i| blake3::hash(format!("leaf-{i}-of-{size}").as_bytes()))
                .collect();
            let tree = MerkleTree::build(&leaves);
            let root_bytes: [u8; 32] = *tree.root().as_bytes();

            for (i, leaf) in leaves.iter().enumerate() {
                let proof_bytes = tree.proof_bytes(i as u32);
                let leaf_bytes: [u8; 32] = *leaf.as_bytes();
                assert!(
                    verify_merkle_proof_bytes(&leaf_bytes, i as u32, &proof_bytes, &root_bytes),
                    "verify failed for size={size} index={i} (proof_len={})",
                    proof_bytes.len()
                );
            }
        }
    }

    // ── Strict V2 verifier (verify_merkle_proof_bytes_for_tree) ────────────────

    /// Reviewer-required: total_leaves == 0 → reject regardless of inputs.
    #[test]
    fn strict_verifier_rejects_empty_tree() {
        let any_hash = [0xAB; 32];
        assert!(
            !verify_merkle_proof_bytes_for_tree(&any_hash, 0, &[], &any_hash, 0),
            "total_leaves=0 must always reject"
        );
        // Even with a "valid-looking" chunk_index and root, must reject.
        assert!(
            !verify_merkle_proof_bytes_for_tree(&any_hash, 5, &[any_hash], &any_hash, 0)
        );
    }

    /// Reviewer-required: chunk_index out of range → reject.
    #[test]
    fn strict_verifier_rejects_out_of_range_index() {
        use crate::merkle::MerkleTree;

        // Build a 4-leaf tree, then probe indices 4, 5, 1000, u32::MAX.
        let leaves: Vec<blake3::Hash> = (0..4).map(|i| blake3::hash(&[i as u8; 16])).collect();
        let tree = MerkleTree::build(&leaves);
        let root: [u8; 32] = *tree.root().as_bytes();
        // A real, well-formed proof for a real index — but pretend it's
        // for an out-of-range index. Must still reject because of the
        // bounds check.
        let real_proof = tree.proof_bytes(0);
        let real_leaf: [u8; 32] = *leaves[0].as_bytes();
        for bad_index in [4u32, 5, 100, u32::MAX] {
            assert!(
                !verify_merkle_proof_bytes_for_tree(&real_leaf, bad_index, &real_proof, &root, 4),
                "chunk_index={bad_index} for total_leaves=4 must reject"
            );
        }
    }

    /// Reviewer-required: wrong proof length → reject. Covers both
    /// too-short and too-long paths against a 5-leaf tree (canonical
    /// depth = 3).
    #[test]
    fn strict_verifier_rejects_wrong_proof_length() {
        use crate::merkle::MerkleTree;

        let leaves: Vec<blake3::Hash> = (0..5).map(|i| blake3::hash(&[i as u8; 16])).collect();
        let tree = MerkleTree::build(&leaves);
        let root: [u8; 32] = *tree.root().as_bytes();
        let real_proof = tree.proof_bytes(2);
        let real_leaf: [u8; 32] = *leaves[2].as_bytes();
        assert_eq!(real_proof.len(), 3, "5-leaf tree depth = 3");

        // Sanity: the real proof at the real length passes.
        assert!(
            verify_merkle_proof_bytes_for_tree(&real_leaf, 2, &real_proof, &root, 5),
            "real proof must verify"
        );

        // Truncate to length 2 (too short).
        let too_short = real_proof[..2].to_vec();
        assert!(
            !verify_merkle_proof_bytes_for_tree(&real_leaf, 2, &too_short, &root, 5),
            "proof shorter than expected_depth(5)=3 must reject"
        );

        // Empty proof against a multi-leaf tree.
        assert!(
            !verify_merkle_proof_bytes_for_tree(&real_leaf, 2, &[], &root, 5),
            "empty proof against total_leaves=5 must reject"
        );

        // Append a junk sibling (too long).
        let mut too_long = real_proof.clone();
        too_long.push([0xFF; 32]);
        assert!(
            !verify_merkle_proof_bytes_for_tree(&real_leaf, 2, &too_long, &root, 5),
            "proof longer than expected_depth(5)=3 must reject"
        );
    }

    /// Reviewer-required: odd-tree (size 5, exercises the duplication
    /// codepath at one level) — every valid (leaf, proof, root) triple
    /// from the canonical builder verifies via the strict path.
    #[test]
    fn strict_verifier_accepts_odd_tree_valid_proofs() {
        use crate::merkle::MerkleTree;

        let leaves: Vec<blake3::Hash> = (0..5).map(|i| blake3::hash(&[i as u8; 16])).collect();
        let tree = MerkleTree::build(&leaves);
        let root: [u8; 32] = *tree.root().as_bytes();
        for (i, leaf) in leaves.iter().enumerate() {
            let proof = tree.proof_bytes(i as u32);
            let leaf_bytes: [u8; 32] = *leaf.as_bytes();
            assert!(
                verify_merkle_proof_bytes_for_tree(&leaf_bytes, i as u32, &proof, &root, 5),
                "5-leaf tree index {i} must verify via strict path"
            );
        }
    }

    /// Single-leaf tree: empty proof + root == leaf must verify; any
    /// other shape must reject. This is the boundary where the strict
    /// verifier short-circuits before walking the proof.
    #[test]
    fn strict_verifier_handles_single_leaf_tree() {
        let leaf: [u8; 32] = *blake3::hash(b"only chunk").as_bytes();
        let other: [u8; 32] = *blake3::hash(b"other").as_bytes();

        // Happy path: 1-leaf tree, root == leaf, empty proof.
        assert!(
            verify_merkle_proof_bytes_for_tree(&leaf, 0, &[], &leaf, 1)
        );

        // Wrong root → reject.
        assert!(
            !verify_merkle_proof_bytes_for_tree(&leaf, 0, &[], &other, 1)
        );

        // Non-empty proof against single-leaf tree → reject (depth must be 0).
        assert!(
            !verify_merkle_proof_bytes_for_tree(&leaf, 0, &[other], &leaf, 1)
        );

        // chunk_index >= total_leaves=1 → reject.
        assert!(
            !verify_merkle_proof_bytes_for_tree(&leaf, 1, &[], &leaf, 1)
        );
    }

    /// Sanity: the depth helper agrees with `MerkleTree::depth()` across
    /// a range of leaf counts. If these ever drift apart, the strict
    /// verifier will reject every honest proof — catastrophic — so we
    /// pin the equivalence here.
    #[test]
    fn strict_verifier_depth_matches_merkle_tree_depth() {
        use crate::merkle::MerkleTree;

        // depth(0) is undefined for proof purposes; skip 0 (caller rejects).
        for n in [1usize, 2, 3, 4, 5, 7, 8, 9, 16, 17, 33, 1024] {
            let leaves: Vec<blake3::Hash> =
                (0..n).map(|i| blake3::hash(&[i as u8; 4])).collect();
            let tree = MerkleTree::build(&leaves);
            let canonical_depth = tree.depth();
            let strict_depth = expected_proof_depth(n as u32);
            assert_eq!(
                canonical_depth, strict_depth,
                "MerkleTree::depth({n}) = {canonical_depth} != expected_proof_depth({n}) = {strict_depth}"
            );
        }
    }

    /// Tampering with any byte of either the leaf, the proof, or the
    /// root must cause verification to fail.
    #[test]
    fn verify_merkle_proof_bytes_tamper_detection() {
        use crate::merkle::MerkleTree;

        let leaves: Vec<blake3::Hash> = (0..5).map(|i| blake3::hash(&[i as u8; 32])).collect();
        let tree = MerkleTree::build(&leaves);
        let root_bytes: [u8; 32] = *tree.root().as_bytes();
        let proof_bytes = tree.proof_bytes(2);
        let leaf_bytes: [u8; 32] = *leaves[2].as_bytes();

        // Sanity: untouched proof verifies.
        assert!(verify_merkle_proof_bytes(&leaf_bytes, 2, &proof_bytes, &root_bytes));

        // Flip a bit in the leaf.
        let mut bad_leaf = leaf_bytes;
        bad_leaf[0] ^= 0x01;
        assert!(!verify_merkle_proof_bytes(&bad_leaf, 2, &proof_bytes, &root_bytes));

        // Flip a bit in the root.
        let mut bad_root = root_bytes;
        bad_root[0] ^= 0x01;
        assert!(!verify_merkle_proof_bytes(&leaf_bytes, 2, &proof_bytes, &bad_root));

        // Flip a bit in the proof's first sibling.
        if !proof_bytes.is_empty() {
            let mut bad_proof = proof_bytes.clone();
            bad_proof[0][0] ^= 0x01;
            assert!(!verify_merkle_proof_bytes(&leaf_bytes, 2, &bad_proof, &root_bytes));
        }

        // Wrong chunk_index must fail too.
        assert!(!verify_merkle_proof_bytes(&leaf_bytes, 3, &proof_bytes, &root_bytes));
    }
}
