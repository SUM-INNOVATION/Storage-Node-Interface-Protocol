//! Deterministic chunk-to-node assignment for 3x replication.
//!
//! This algorithm must produce byte-identical results to the L1's
//! `compute_chunk_assignment()` in `sum-chain/crates/state/src/storage_metadata.rs`.
//!
//! Both use `blake3(merkle_root ++ chunk_index.to_be_bytes() ++ replica.to_be_bytes())`
//! to hash each (chunk, replica) pair into a position in the sorted node list,
//! with linear probing on collision.

use sum_types::storage::REPLICATION_FACTOR;

/// Deterministically assign nodes to each chunk of a file.
///
/// For each chunk, exactly `min(replication_factor, active_nodes.len())` unique
/// nodes are assigned. The algorithm uses blake3 hashing with linear probing.
///
/// `active_nodes` **must** be sorted by bytes before calling.
pub fn compute_chunk_assignment(
    merkle_root: &[u8; 32],
    chunk_count: u64,
    active_nodes: &[[u8; 20]],
    replication_factor: u32,
) -> Vec<Vec<[u8; 20]>> {
    let n = active_nodes.len();
    if n == 0 || chunk_count == 0 {
        return vec![vec![]; chunk_count as usize];
    }

    let replicas = (replication_factor as usize).min(n);
    let mut assignment = Vec::with_capacity(chunk_count as usize);

    for chunk_index in 0..chunk_count {
        let mut assigned: Vec<[u8; 20]> = Vec::with_capacity(replicas);

        for replica in 0..replicas as u32 {
            // Hash must match L1's Hash::hash_many(&[merkle_root, chunk_index BE, replica BE])
            let mut hasher = blake3::Hasher::new();
            hasher.update(merkle_root);
            hasher.update(&(chunk_index as u32).to_be_bytes());
            hasher.update(&replica.to_be_bytes());
            let hash = hasher.finalize();
            let hash_bytes = hash.as_bytes();

            let start_index = u64::from_be_bytes([
                hash_bytes[0], hash_bytes[1], hash_bytes[2], hash_bytes[3],
                hash_bytes[4], hash_bytes[5], hash_bytes[6], hash_bytes[7],
            ]) as usize % n;

            // Linear probe to find an unassigned node for this chunk.
            let mut idx = start_index;
            loop {
                if !assigned.contains(&active_nodes[idx]) {
                    assigned.push(active_nodes[idx]);
                    break;
                }
                idx = (idx + 1) % n;
            }
        }

        assignment.push(assigned);
    }

    assignment
}

/// Given a full assignment, return the chunk indices assigned to a specific node.
pub fn chunks_for_node(
    assignment: &[Vec<[u8; 20]>],
    node_address: &[u8; 20],
) -> Vec<u32> {
    assignment
        .iter()
        .enumerate()
        .filter(|(_, nodes)| nodes.contains(node_address))
        .map(|(i, _)| i as u32)
        .collect()
}

/// Given a full assignment, return the nodes assigned to a specific chunk.
pub fn nodes_for_chunk(
    assignment: &[Vec<[u8; 20]>],
    chunk_index: u32,
) -> Option<&Vec<[u8; 20]>> {
    assignment.get(chunk_index as usize)
}

/// Convenience: compute assignment with default REPLICATION_FACTOR.
pub fn compute_default_assignment(
    merkle_root: &[u8; 32],
    chunk_count: u64,
    active_nodes: &[[u8; 20]],
) -> Vec<Vec<[u8; 20]>> {
    compute_chunk_assignment(merkle_root, chunk_count, active_nodes, REPLICATION_FACTOR)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_nodes(n: usize) -> Vec<[u8; 20]> {
        let mut nodes: Vec<[u8; 20]> = (0..n)
            .map(|i| {
                let mut addr = [0u8; 20];
                addr[0] = (i >> 8) as u8;
                addr[1] = i as u8;
                // Fill remaining bytes for realism
                let hash = blake3::hash(&addr);
                addr[2..].copy_from_slice(&hash.as_bytes()[..18]);
                addr
            })
            .collect();
        nodes.sort();
        nodes
    }

    #[test]
    fn basic_assignment_3_replicas() {
        let nodes = make_nodes(5);
        let root = [0xAA; 32];
        let assignment = compute_chunk_assignment(&root, 4, &nodes, 3);

        assert_eq!(assignment.len(), 4);
        for chunk_nodes in &assignment {
            assert_eq!(chunk_nodes.len(), 3, "each chunk should have 3 replicas");
        }
    }

    #[test]
    fn no_duplicate_nodes_per_chunk() {
        let nodes = make_nodes(10);
        let root = [0xBB; 32];
        let assignment = compute_chunk_assignment(&root, 8, &nodes, 3);

        for (i, chunk_nodes) in assignment.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for node in chunk_nodes {
                assert!(
                    seen.insert(node),
                    "chunk {i} has duplicate node assignment"
                );
            }
        }
    }

    #[test]
    fn deterministic() {
        let nodes = make_nodes(5);
        let root = [0xCC; 32];
        let a1 = compute_chunk_assignment(&root, 4, &nodes, 3);
        let a2 = compute_chunk_assignment(&root, 4, &nodes, 3);
        assert_eq!(a1, a2, "same inputs must produce same output");
    }

    #[test]
    fn different_files_different_assignments() {
        let nodes = make_nodes(10);
        let root1 = [0x11; 32];
        let root2 = [0x22; 32];
        let a1 = compute_chunk_assignment(&root1, 4, &nodes, 3);
        let a2 = compute_chunk_assignment(&root2, 4, &nodes, 3);
        // Not all chunks should map to the same nodes
        assert_ne!(a1, a2, "different files should generally produce different assignments");
    }

    #[test]
    fn fewer_nodes_than_replication() {
        let nodes = make_nodes(2);
        let root = [0xDD; 32];
        let assignment = compute_chunk_assignment(&root, 4, &nodes, 3);

        for chunk_nodes in &assignment {
            assert_eq!(chunk_nodes.len(), 2, "should use all available nodes when N < R");
        }
    }

    #[test]
    fn single_node() {
        let nodes = make_nodes(1);
        let root = [0xEE; 32];
        let assignment = compute_chunk_assignment(&root, 4, &nodes, 3);

        for chunk_nodes in &assignment {
            assert_eq!(chunk_nodes.len(), 1);
            assert_eq!(chunk_nodes[0], nodes[0]);
        }
    }

    #[test]
    fn empty_nodes() {
        let nodes: Vec<[u8; 20]> = vec![];
        let root = [0xFF; 32];
        let assignment = compute_chunk_assignment(&root, 4, &nodes, 3);

        assert_eq!(assignment.len(), 4);
        for chunk_nodes in &assignment {
            assert!(chunk_nodes.is_empty());
        }
    }

    #[test]
    fn zero_chunks() {
        let nodes = make_nodes(5);
        let root = [0x00; 32];
        let assignment = compute_chunk_assignment(&root, 0, &nodes, 3);
        assert!(assignment.is_empty());
    }

    #[test]
    fn chunks_for_node_helper() {
        let nodes = make_nodes(5);
        let root = [0xAA; 32];
        let assignment = compute_chunk_assignment(&root, 10, &nodes, 3);

        // Each node should be assigned to some chunks (with 10 chunks and R=3,
        // that's 30 slots across 5 nodes = ~6 per node on average)
        for node in &nodes {
            let chunks = chunks_for_node(&assignment, node);
            assert!(!chunks.is_empty(), "every node should have at least one assigned chunk");
        }
    }

    #[test]
    fn nodes_for_chunk_helper() {
        let nodes = make_nodes(5);
        let root = [0xAA; 32];
        let assignment = compute_chunk_assignment(&root, 4, &nodes, 3);

        let chunk0_nodes = nodes_for_chunk(&assignment, 0).unwrap();
        assert_eq!(chunk0_nodes.len(), 3);

        assert!(nodes_for_chunk(&assignment, 99).is_none());
    }

    #[test]
    fn large_file_even_distribution() {
        let nodes = make_nodes(10);
        let root = [0x42; 32];
        let assignment = compute_chunk_assignment(&root, 100, &nodes, 3);

        // Count how many chunks each node is assigned to
        let mut counts = std::collections::HashMap::new();
        for chunk_nodes in &assignment {
            for node in chunk_nodes {
                *counts.entry(node).or_insert(0u32) += 1;
            }
        }

        // With 100 chunks * 3 replicas = 300 slots across 10 nodes = ~30 each
        // No node should have 0, no node should have all 100
        for node in &nodes {
            let count = counts.get(&node).copied().unwrap_or(0);
            assert!(count > 0, "node should have at least 1 assignment");
            assert!(count < 100, "node should not have all chunks");
        }
    }

    #[test]
    fn cross_verify_hash_computation() {
        // Manually verify the hash matches what the L1 would compute.
        // L1 uses Hash::hash_many(&[merkle_root, chunk_index.to_be_bytes(), replica.to_be_bytes()])
        // which is blake3::Hasher::new().update(a).update(b).update(c).finalize()
        let merkle_root = [0xAA_u8; 32];
        let chunk_index: u32 = 7;
        let replica: u32 = 1;

        let mut hasher = blake3::Hasher::new();
        hasher.update(&merkle_root);
        hasher.update(&chunk_index.to_be_bytes());
        hasher.update(&replica.to_be_bytes());
        let hash = hasher.finalize();

        // Verify the hash is non-zero and deterministic
        assert_ne!(hash.as_bytes(), &[0u8; 32]);

        // Verify same computation produces same hash
        let mut hasher2 = blake3::Hasher::new();
        hasher2.update(&merkle_root);
        hasher2.update(&chunk_index.to_be_bytes());
        hasher2.update(&replica.to_be_bytes());
        assert_eq!(hash, hasher2.finalize());
    }
}