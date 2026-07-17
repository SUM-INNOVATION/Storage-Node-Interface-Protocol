//! S4 PR-A equivalence tests: prove SNIP's wrapped assignment KDF and its
//! Merkle-root construction stay byte-identical to the published
//! `sumchain-wire 0.1.1` crate they now delegate to.
//!
//! These are ADD-ONLY. They do not replace the Appendix-C conformance
//! vectors in `assignment_v2_conformance.rs` (the byte tripwire) — they
//! cross-check SNIP's public surface against the wire crate directly.

use sum_store::MerkleTree;
use sum_store::assignment_v2;

use sumchain_wire::Address;
use sumchain_wire::Hash;
use sumchain_wire::storage_metadata::assigned_archives as wire_assigned_archives;

// ── Fixture construction (matches Appendix C "Inputs") ────────────────────────

fn merkle_root(i: usize) -> [u8; 32] {
    *blake3::hash(format!("snip-v2-test-file-{}", i + 1).as_bytes()).as_bytes()
}

fn archive(j: usize) -> [u8; 20] {
    let h = blake3::hash(format!("snip-v2-archive-{}", j + 1).as_bytes());
    let mut a = [0u8; 20];
    a.copy_from_slice(&h.as_bytes()[..20]);
    a
}

/// Run the SNIP wrapper and the wire function against the same input and
/// assert the byte outputs are identical.
fn assert_equiv(root: &[u8; 32], snapshot: &[[u8; 20]], chunk_index: u32, r: u32) {
    let snip = assignment_v2::assigned_archives(root, snapshot, chunk_index, r);

    let wire_root = Hash::new(*root);
    let wire_snap: Vec<Address> = snapshot.iter().map(|a| Address::new(*a)).collect();
    let wire: Vec<[u8; 20]> = wire_assigned_archives(&wire_root, &wire_snap, chunk_index, r)
        .into_iter()
        .map(|a| *a.as_bytes())
        .collect();

    assert_eq!(
        snip, wire,
        "assigned_archives divergence: chunk_index={chunk_index}, R={r}"
    );
}

// ── Assignment equivalence over the Appendix-C grid + edge cases ──────────────

#[test]
fn assigned_archives_matches_wire_over_appendix_c_grid() {
    let snapshot = vec![archive(0), archive(1), archive(2), archive(3), archive(4)];

    // Full cross-product of the Appendix-C roots, a spread of chunk indices
    // (including the u32::MAX boundary the task calls out), and every R the
    // conformance grid exercises plus R=7 (clamp) and R=0 (empty).
    for file_idx in 0..3usize {
        let root = merkle_root(file_idx);
        for &chunk_index in &[0u32, 1, 7, 42, 1000, u32::MAX] {
            for &r in &[0u32, 1, 3, 5, 7] {
                assert_equiv(&root, &snapshot, chunk_index, r);
            }
        }
    }
}

#[test]
fn assigned_archives_matches_wire_for_duplicate_address_snapshot() {
    // Duplicate + out-of-order snapshot: both implementations must sort and
    // dedup to the same canonical set and emit identical bytes.
    let dupes = vec![
        archive(3),
        archive(0),
        archive(0),
        archive(2),
        archive(4),
        archive(1),
        archive(2),
        archive(3),
    ];
    for file_idx in 0..3usize {
        let root = merkle_root(file_idx);
        for &chunk_index in &[0u32, 42, u32::MAX] {
            for &r in &[1u32, 3, 5, 7] {
                assert_equiv(&root, &dupes, chunk_index, r);
            }
        }
    }
}

#[test]
fn assigned_archives_matches_wire_for_empty_snapshot() {
    let root = merkle_root(0);
    for &chunk_index in &[0u32, u32::MAX] {
        for &r in &[0u32, 1, 3] {
            assert_equiv(&root, &[], chunk_index, r);
        }
    }
}

// ── Merkle-root equivalence: SNIP MerkleTree vs wire Hash::merkle_root ─────────

#[test]
fn merkle_root_matches_wire_across_leaf_counts() {
    // Odd / even / single / empty coverage per the task: {0,1,2,3,17}.
    for &n in &[0usize, 1, 2, 3, 17] {
        let leaves: Vec<blake3::Hash> = (0..n)
            .map(|i| blake3::hash(format!("wire-equiv-leaf-{i}").as_bytes()))
            .collect();

        let snip_root = MerkleTree::build(&leaves).root();

        let wire_leaves: Vec<Hash> = leaves.iter().map(|h| Hash::new(*h.as_bytes())).collect();
        let wire_root = Hash::merkle_root(&wire_leaves);

        assert_eq!(
            snip_root.as_bytes(),
            wire_root.as_bytes(),
            "merkle root divergence at leaf_count={n}"
        );
    }
}
