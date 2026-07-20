//! V2 deterministic chunk-to-archive assignment.
//!
//! Per chain plan v3.2 §3.6 — rendezvous-hash via the chain's frozen
//! BLAKE3 KDF. This module is a thin, byte-identical adapter over the
//! published [`sumchain_wire::storage_metadata`] crate — the single home
//! of the chain's assignment scorer AND its assignment-set functions — so
//! SNIP and the chain cannot drift.
//!
//! ```text
//! candidates = snapshot, deduped + sorted by 20-byte address ascending
//! assigned   = candidates sorted by (score asc, address asc) → take top R
//! ```
//!
//! This is **consensus-adjacent** code. The chain executor, the
//! `storage_getAssignmentCoverageV2` RPC, and SNIP push logic all
//! recompute the same assignment. Any divergence (wrong context string,
//! wrong endianness, wrong `blake3` API variant, wrong tie-break order)
//! desynchronises SNIP from the chain and breaks both push validation
//! (chunks rejected by archives that disagree about whether they were
//! assigned) and `ActivateFileV2` validity. SNIP therefore holds **no**
//! copy of the scorer: the raw per-tuple score comes from
//! [`sumchain_wire::storage_metadata::assignment_score`] and the
//! assignment sets from the wire crate's `assigned_archives*` functions.
//! These wrappers only convert SNIP's `[u8; 20]` ↔ wire `Address` and
//! `[u8; 32]` ↔ wire `Hash`, reusing the wire crate's own sort/dedup/clamp
//! and `(score, address)` ordering.
//!
//! Conformance vectors live in
//! [`crates/sum-store/tests/assignment_v2_conformance.rs`] and pull
//! directly from chain plan Appendix C. They score tuples through the wire
//! crate's public `assignment_score` and assert the exact Appendix-C
//! bytes, so any drift from the chain's hashing lands there and not in
//! production.
//!
//! V1's [`crate::assignment::compute_chunk_assignment`] uses a linear-
//! probing hash, **NOT** this rendezvous-hash function — they are
//! algorithmically distinct and produce different outputs. Do not
//! substitute one for the other.

use std::collections::BTreeSet;

use sumchain_wire::Address;
use sumchain_wire::Hash;
use sumchain_wire::storage_metadata::{
    assigned_archives as wire_assigned_archives,
    assigned_archives_presorted as wire_assigned_archives_presorted,
};

/// Canonicalize an archive snapshot into the wire `Address` form the
/// batch entry points feed to [`wire_assigned_archives_presorted`]:
/// dedup + sort by 20-byte address ascending. `Address` derives `Ord`
/// over its inner `[u8; 20]`, so `sort` + `dedup` yields exactly the
/// ascending-byte-order canonical form the chain expects (identical to
/// the wire's internal `sort_by(as_bytes)` + `dedup_by(as_bytes ==)`).
fn presorted_addresses(snapshot: &[[u8; 20]]) -> Vec<Address> {
    let mut addrs: Vec<Address> = snapshot.iter().map(|a| Address::new(*a)).collect();
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// Compute the top-`replication_factor` archives assigned to a single
/// chunk_index. Output is ordered ascending by `(score, address)`.
///
/// `R` is clamped to the canonical snapshot size (so `R = 7` against a
/// 5-archive snapshot returns 5 archives, identical to `R = 5`).
pub fn assigned_archives(
    merkle_root: &[u8; 32],
    snapshot: &[[u8; 20]],
    chunk_index: u32,
    replication_factor: u32,
) -> Vec<[u8; 20]> {
    // Thin wrapper over the wire crate. `wire_assigned_archives` performs
    // its own sort+dedup+clamp and the same `(score, address)` ordering, so
    // the raw (unsorted) converted snapshot is passed straight through.
    let root = Hash::new(*merkle_root);
    let addrs: Vec<Address> = snapshot.iter().map(|a| Address::new(*a)).collect();
    wire_assigned_archives(&root, &addrs, chunk_index, replication_factor)
        .into_iter()
        .map(|a| *a.as_bytes())
        .collect()
}

/// Compute the full assignment for every chunk in `[0, chunk_count)`.
///
/// O(chunk_count × snapshot_size) hashes — for the upper-bound case of
/// `chunk_count = 1_048_576` and `snapshot.len() = 100` archives this
/// is ~10⁸ BLAKE3 hashes; runs in seconds. Callers ingesting smaller
/// files (typical) finish in milliseconds.
pub fn compute_assignment_v2(
    merkle_root: &[u8; 32],
    chunk_count: u32,
    snapshot: &[[u8; 20]],
    replication_factor: u32,
) -> Vec<Vec<[u8; 20]>> {
    // Canonicalize once, reuse for every chunk to avoid repeated sort+dedup —
    // this is exactly what `assigned_archives_presorted` expects.
    let root = Hash::new(*merkle_root);
    let presorted = presorted_addresses(snapshot);

    (0..chunk_count)
        .map(|chunk_index| {
            wire_assigned_archives_presorted(&root, &presorted, chunk_index, replication_factor)
                .into_iter()
                .map(|a| *a.as_bytes())
                .collect()
        })
        .collect()
}

/// Compute the set of chunk indices assigned to a specific archive
/// address across the whole `[0, chunk_count)` range.
///
/// Used by the assignment-attestor: an archive that just finished
/// receiving pushes for file `R` calls this to know exactly which
/// chunk indices to OR into its `AcceptAssignmentV2.chunk_indices`.
pub fn chunks_for_archive_v2(
    merkle_root: &[u8; 32],
    chunk_count: u32,
    snapshot: &[[u8; 20]],
    replication_factor: u32,
    archive: &[u8; 20],
) -> BTreeSet<u32> {
    let root = Hash::new(*merkle_root);
    let presorted = presorted_addresses(snapshot);
    let target = Address::new(*archive);
    // Fast-out: if the archive isn't even in the canonical snapshot, it
    // can't be assigned to anything for this file. `presorted` is sorted,
    // so binary_search is valid.
    if presorted.binary_search(&target).is_err() {
        return BTreeSet::new();
    }

    let mut chunks = BTreeSet::new();
    for chunk_index in 0..chunk_count {
        let assigned =
            wire_assigned_archives_presorted(&root, &presorted, chunk_index, replication_factor);
        if assigned.iter().any(|a| a.as_bytes() == archive) {
            chunks.insert(chunk_index);
        }
    }
    chunks
}

// ── Inline unit tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_yields_empty_assignment() {
        let root = [0u8; 32];
        assert!(assigned_archives(&root, &[], 0, 3).is_empty());
        assert_eq!(compute_assignment_v2(&root, 4, &[], 3).len(), 4);
        for chunks in compute_assignment_v2(&root, 4, &[], 3) {
            assert!(chunks.is_empty());
        }
    }

    #[test]
    fn replication_factor_clamps_to_snapshot_size() {
        let root = [1u8; 32];
        let snapshot = vec![[0xAA; 20], [0xBB; 20]];
        let assigned = assigned_archives(&root, &snapshot, 0, 7);
        assert_eq!(assigned.len(), 2, "R=7 clamps to snapshot.len()=2");
    }

    #[test]
    fn snapshot_order_is_irrelevant() {
        let root = [2u8; 32];
        let s1 = vec![[0x10; 20], [0x20; 20], [0x30; 20]];
        let s2 = vec![[0x30; 20], [0x10; 20], [0x20; 20]];
        assert_eq!(
            assigned_archives(&root, &s1, 0, 3),
            assigned_archives(&root, &s2, 0, 3),
            "assignment must be invariant to input snapshot ordering"
        );
    }

    #[test]
    fn duplicate_archives_in_snapshot_dedup() {
        let root = [3u8; 32];
        let with_dups = vec![[0x10; 20], [0x10; 20], [0x20; 20]];
        let unique = vec![[0x10; 20], [0x20; 20]];
        assert_eq!(
            assigned_archives(&root, &with_dups, 0, 3),
            assigned_archives(&root, &unique, 0, 3),
            "duplicates must be canonicalized away"
        );
    }

    #[test]
    fn chunks_for_archive_returns_consistent_subset() {
        // For a given file + snapshot, the union of `chunks_for_archive`
        // across all archives must equal R copies of [0, chunk_count) —
        // every chunk is assigned to exactly R archives.
        let root = [4u8; 32];
        let snapshot: Vec<[u8; 20]> = (0..5)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = i;
                a
            })
            .collect();
        let chunk_count = 20;
        let r = 3;

        let mut total_assignments = 0;
        for archive in &snapshot {
            let chunks = chunks_for_archive_v2(&root, chunk_count, &snapshot, r, archive);
            total_assignments += chunks.len();
            // Cross-check: each chunk in the set must independently
            // confirm via assigned_archives.
            for &c in &chunks {
                let assigned = assigned_archives(&root, &snapshot, c, r);
                assert!(
                    assigned.contains(archive),
                    "chunks_for_archive said {archive:?} owns {c}, \
                     but assigned_archives({c}) doesn't include it"
                );
            }
        }
        assert_eq!(
            total_assignments,
            (chunk_count as usize) * (r as usize),
            "every chunk must be assigned to exactly R archives"
        );
    }

    #[test]
    fn archive_outside_snapshot_gets_no_chunks() {
        let root = [5u8; 32];
        let snapshot = vec![[0xAA; 20], [0xBB; 20]];
        let stranger = [0xCC; 20];
        let chunks = chunks_for_archive_v2(&root, 100, &snapshot, 3, &stranger);
        assert!(chunks.is_empty());
    }

    #[test]
    fn changing_chunk_index_changes_assignment() {
        // Different chunk_index against the same snapshot should usually
        // yield a different ordering; this is the basic correctness
        // property that scoring depends on chunk_index.
        let root = [6u8; 32];
        let snapshot: Vec<[u8; 20]> = (0..5)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = i + 1;
                a
            })
            .collect();
        let a0 = assigned_archives(&root, &snapshot, 0, 5);
        let a1 = assigned_archives(&root, &snapshot, 1, 5);
        assert_ne!(a0, a1, "chunk_index must influence the score");
    }
}
