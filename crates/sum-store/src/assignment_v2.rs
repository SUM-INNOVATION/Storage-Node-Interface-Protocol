//! V2 deterministic chunk-to-archive assignment.
//!
//! Per chain plan v3.2 §3.6 — rendezvous-hash via BLAKE3 `derive_key`:
//!
//! ```text
//! score(archive) = u64::from_be_bytes(
//!     blake3::derive_key(
//!         "sumchain SNIP-V2 chunk-assignment v1",
//!         merkle_root(32) || chunk_index_be(4) || archive_address(20)
//!     )[..8]
//! )
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
//! assigned) and `ActivateFileV2` validity.
//!
//! Conformance vectors live in
//! [`crates/sum-store/tests/assignment_v2_conformance.rs`] and pull
//! directly from chain plan Appendix C. Any change to this module that
//! alters byte-level outputs MUST be cross-checked against those
//! vectors before merging.
//!
//! V1's [`crate::assignment::compute_chunk_assignment`] uses a linear-
//! probing hash, **NOT** this rendezvous-hash function — they are
//! algorithmically distinct and produce different outputs. Do not
//! substitute one for the other.

use std::collections::BTreeSet;

/// Domain-separation context for the V2 assignment KDF.
///
/// **Exact bytes — do not modify.** Trailing newline, casing, or
/// "v2" in place of "v1" all break conformance with the chain.
pub const ASSIGNMENT_V2_CONTEXT: &str = "sumchain SNIP-V2 chunk-assignment v1";

/// Score one `(merkle_root, chunk_index, archive_address)` tuple.
///
/// Returns the first 8 bytes of `blake3::derive_key(CTX, …)` interpreted
/// as a big-endian `u64`. Lower scores rank archives higher in the
/// rendezvous-hash ordering.
///
/// Public so conformance tests can assert exact byte values from
/// chain plan Appendix C without going through the assignment-output
/// API.
pub fn score(merkle_root: &[u8; 32], chunk_index: u32, archive: &[u8; 20]) -> u64 {
    // Stack-allocated 56-byte input — no heap, no allocator pressure
    // when computing assignments for many chunks.
    let mut input = [0u8; 32 + 4 + 20];
    input[..32].copy_from_slice(merkle_root);
    input[32..36].copy_from_slice(&chunk_index.to_be_bytes());
    input[36..].copy_from_slice(archive);

    // Note: blake3::derive_key(context, input) is the chain's exact
    // canonical API. Equivalent to `blake3::Hasher::new_derive_key(ctx)
    // .update(input).finalize()`. Do NOT substitute `keyed_hash` (uses
    // a 32-byte key not a context string) or plain `hash` (no domain
    // separation).
    let derived = blake3::derive_key(ASSIGNMENT_V2_CONTEXT, &input);

    u64::from_be_bytes([
        derived[0], derived[1], derived[2], derived[3],
        derived[4], derived[5], derived[6], derived[7],
    ])
}

/// Canonicalize an archive snapshot: dedup + sort by 20-byte address
/// ascending. The chain expects this exact preprocessing before
/// scoring; without it, two snapshots that differ only in input
/// ordering or duplicates would yield different assignments.
fn canonicalize_snapshot(snapshot: &[[u8; 20]]) -> Vec<[u8; 20]> {
    let set: BTreeSet<[u8; 20]> = snapshot.iter().copied().collect();
    set.into_iter().collect()
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
    let canonical = canonicalize_snapshot(snapshot);
    if canonical.is_empty() {
        return Vec::new();
    }
    let r = (replication_factor as usize).min(canonical.len());

    // Score every archive. Capacity = canonical.len() so no realloc.
    let mut scored: Vec<(u64, [u8; 20])> = canonical
        .iter()
        .map(|addr| (score(merkle_root, chunk_index, addr), *addr))
        .collect();

    // Order by (score asc, address asc). Ties resolve by raw address.
    // The canonical sort already happened above, so for any two
    // archives with the same score the BTreeSet ordering survives the
    // stable sort_by. Belt-and-suspenders: explicit tie-break here.
    scored.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));

    scored.into_iter().take(r).map(|(_, addr)| addr).collect()
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
    // Canonicalize once, reuse for every chunk to avoid repeated sort+dedup.
    let canonical = canonicalize_snapshot(snapshot);
    let r = (replication_factor as usize).min(canonical.len());

    (0..chunk_count)
        .map(|chunk_index| {
            if canonical.is_empty() {
                return Vec::new();
            }
            let mut scored: Vec<(u64, [u8; 20])> = canonical
                .iter()
                .map(|addr| (score(merkle_root, chunk_index, addr), *addr))
                .collect();
            scored.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
            scored.into_iter().take(r).map(|(_, addr)| addr).collect()
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
    let canonical = canonicalize_snapshot(snapshot);
    // Fast-out: if the archive isn't even in the canonical snapshot, it
    // can't be assigned to anything for this file.
    if canonical.binary_search(archive).is_err() {
        return BTreeSet::new();
    }
    let r = (replication_factor as usize).min(canonical.len());

    let mut chunks = BTreeSet::new();
    for chunk_index in 0..chunk_count {
        let mut scored: Vec<(u64, [u8; 20])> = canonical
            .iter()
            .map(|addr| (score(merkle_root, chunk_index, addr), *addr))
            .collect();
        scored.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
        if scored.iter().take(r).any(|(_, addr)| addr == archive) {
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
