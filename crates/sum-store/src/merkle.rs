//! BLAKE3 binary Merkle tree builder aligned with the SUM Chain L1.
//!
//! The tree construction matches the L1's `Hash::merkle_root()` exactly:
//! - When a level has an odd number of nodes, the last node is **duplicated**
//!   (not promoted). See `sum-chain/crates/primitives/src/hash.rs:98`.
//! - Hash combiner: `blake3(left_32_bytes || right_32_bytes)`.
//!
//! Proof generation produces sibling hashes bottom-up, matching the L1's
//! `verify_merkle_proof()` at `sum-chain/crates/state/src/storage_metadata.rs:84-110`.

/// A binary Merkle tree over BLAKE3 chunk hashes.
///
/// `levels[0]` contains the leaf hashes, `levels[last]` contains `[root]`.
pub struct MerkleTree {
    levels: Vec<Vec<blake3::Hash>>,
}

impl MerkleTree {
    /// Build a Merkle tree from leaf hashes.
    ///
    /// When a level has an odd count, the last node is duplicated to form a
    /// pair — matching the L1's `Hash::merkle_root()` behaviour.
    pub fn build(leaf_hashes: &[blake3::Hash]) -> Self {
        if leaf_hashes.is_empty() {
            return Self { levels: vec![] };
        }
        if leaf_hashes.len() == 1 {
            return Self { levels: vec![leaf_hashes.to_vec()] };
        }

        let mut levels = vec![leaf_hashes.to_vec()];
        let mut current = leaf_hashes.to_vec();

        while current.len() > 1 {
            let mut next = Vec::with_capacity((current.len() + 1) / 2);
            for pair in current.chunks(2) {
                let left = &pair[0];
                let right = pair.get(1).unwrap_or(left); // duplicate last if odd
                next.push(hash_pair(left, right));
            }
            levels.push(next.clone());
            current = next;
        }

        Self { levels }
    }

    /// The Merkle root hash. Sent to the L1 as the file's identity.
    ///
    /// Returns the zero hash for an empty tree (no leaves).
    pub fn root(&self) -> blake3::Hash {
        self.levels
            .last()
            .and_then(|level| level.first().copied())
            .unwrap_or_else(|| blake3::Hash::from([0u8; 32]))
    }

    /// Tree depth (number of levels above the leaves).
    ///
    /// - 0 leaves → 0
    /// - 1 leaf → 0 (root IS the leaf)
    /// - 2+ leaves → `ceil(log2(leaf_count))`
    pub fn depth(&self) -> usize {
        if self.levels.len() <= 1 {
            0
        } else {
            self.levels.len() - 1
        }
    }

    /// Number of leaf nodes in the tree.
    pub fn leaf_count(&self) -> usize {
        self.levels.first().map_or(0, |l| l.len())
    }

    /// Generate a Merkle proof (sibling hashes, bottom-up) for a given chunk.
    ///
    /// The returned `Vec<blake3::Hash>` is ordered from the leaf level upward,
    /// matching the L1's `verify_merkle_proof()` expectations.
    pub fn generate_proof(&self, chunk_index: u32) -> Vec<blake3::Hash> {
        if self.levels.len() <= 1 {
            return vec![]; // single leaf or empty — no proof needed
        }

        let mut proof = Vec::with_capacity(self.depth());
        let mut idx = chunk_index as usize;

        // Walk from leaf level (0) up to one below the root
        for level in 0..self.levels.len() - 1 {
            let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            let sibling = if sibling_idx < self.levels[level].len() {
                self.levels[level][sibling_idx]
            } else {
                // Out of bounds means the node was duplicated (odd level)
                self.levels[level][idx]
            };
            proof.push(sibling);
            idx /= 2;
        }

        proof
    }
}

/// Combine two 32-byte hashes into a parent hash.
///
/// Uses `blake3(left || right)` — equivalent to the L1's `Hash::hash_many`.
fn hash_pair(left: &blake3::Hash, right: &blake3::Hash) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    hasher.finalize()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::verify_merkle_proof;

    fn make_leaf(data: &[u8]) -> blake3::Hash {
        blake3::hash(data)
    }

    #[test]
    fn empty_tree() {
        let tree = MerkleTree::build(&[]);
        assert_eq!(tree.root(), blake3::Hash::from([0u8; 32]));
        assert_eq!(tree.depth(), 0);
        assert_eq!(tree.leaf_count(), 0);
    }

    #[test]
    fn single_leaf() {
        let h = make_leaf(b"only chunk");
        let tree = MerkleTree::build(&[h]);
        assert_eq!(tree.root(), h);
        assert_eq!(tree.depth(), 0);
        assert!(tree.generate_proof(0).is_empty());
    }

    #[test]
    fn two_leaves() {
        let h0 = make_leaf(b"chunk 0");
        let h1 = make_leaf(b"chunk 1");
        let tree = MerkleTree::build(&[h0, h1]);

        assert_eq!(tree.depth(), 1);
        assert_eq!(tree.root(), hash_pair(&h0, &h1));

        let proof0 = tree.generate_proof(0);
        assert_eq!(proof0.len(), 1);
        assert_eq!(proof0[0], h1);

        let proof1 = tree.generate_proof(1);
        assert_eq!(proof1.len(), 1);
        assert_eq!(proof1[0], h0);
    }

    #[test]
    fn three_leaves_odd_duplication() {
        let h0 = make_leaf(b"chunk 0");
        let h1 = make_leaf(b"chunk 1");
        let h2 = make_leaf(b"chunk 2");
        let tree = MerkleTree::build(&[h0, h1, h2]);

        assert_eq!(tree.depth(), 2);

        // Level 1: [H(h0,h1), H(h2,h2)]  — h2 duplicated per L1 rule
        let l1_0 = hash_pair(&h0, &h1);
        let l1_1 = hash_pair(&h2, &h2);
        let expected_root = hash_pair(&l1_0, &l1_1);
        assert_eq!(tree.root(), expected_root);

        // Verify proofs for all chunks
        let proof0 = tree.generate_proof(0);
        assert_eq!(proof0, vec![h1, l1_1]);

        let proof2 = tree.generate_proof(2);
        assert_eq!(proof2, vec![h2, l1_0]); // sibling is itself (duplicated)
    }

    #[test]
    fn four_leaves_power_of_two() {
        let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
        let tree = MerkleTree::build(&leaves);
        assert_eq!(tree.depth(), 2);
        assert_eq!(tree.leaf_count(), 4);
    }

    #[test]
    fn five_leaves_depth_three() {
        let leaves: Vec<blake3::Hash> = (0..5u8).map(|i| make_leaf(&[i])).collect();
        let tree = MerkleTree::build(&leaves);
        assert_eq!(tree.depth(), 3);
    }

    #[test]
    fn cross_validate_all_proofs_with_l1_verifier() {
        // Test various tree sizes including odd counts
        for n in 1..=17u32 {
            let leaves: Vec<blake3::Hash> =
                (0..n).map(|i| make_leaf(&i.to_le_bytes())).collect();
            let tree = MerkleTree::build(&leaves);
            let root = tree.root();

            for idx in 0..n {
                let proof = tree.generate_proof(idx);
                assert!(
                    verify_merkle_proof(&leaves[idx as usize], idx, &proof, &root),
                    "proof failed: n={n}, idx={idx}"
                );
            }
        }
    }

    #[test]
    fn wrong_index_fails_verification() {
        let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
        let tree = MerkleTree::build(&leaves);
        let root = tree.root();

        // Proof for chunk 0 should fail if we claim it's chunk 1
        let proof0 = tree.generate_proof(0);
        assert!(!verify_merkle_proof(&leaves[0], 1, &proof0, &root));
    }

    #[test]
    fn wrong_hash_fails_verification() {
        let leaves: Vec<blake3::Hash> = (0..4u8).map(|i| make_leaf(&[i])).collect();
        let tree = MerkleTree::build(&leaves);
        let root = tree.root();

        let proof0 = tree.generate_proof(0);
        let fake_hash = make_leaf(b"fake");
        assert!(!verify_merkle_proof(&fake_hash, 0, &proof0, &root));
    }

    #[test]
    fn depth_matches_l1_formula() {
        // L1 formula: for chunk_count > 1, depth = 64 - (chunk_count - 1).leading_zeros()
        for n in 2..=64u32 {
            let leaves: Vec<blake3::Hash> =
                (0..n).map(|i| make_leaf(&i.to_le_bytes())).collect();
            let tree = MerkleTree::build(&leaves);
            let expected = 64 - ((n as u64) - 1).leading_zeros() as usize;
            assert_eq!(
                tree.depth(),
                expected,
                "depth mismatch for n={n}: got {}, expected {expected}",
                tree.depth()
            );
        }
    }
}
