//! Conformance vectors for `assignment_v2`, lifted byte-for-byte from
//! chain plan v3.2 Appendix C.
//!
//! These tests are **mandatory before any V2 ingest / push / attestation
//! code can rely on assignment_v2 outputs**: a divergence here means
//! the SNIP node and the chain disagree about which archives are
//! assigned to which chunks, which silently breaks push validation and
//! `ActivateFileV2` validity. If any test in this file fails, do not
//! merge — fix the assignment function first.
//!
//! The test fixture inputs are constructed from deterministic public
//! recipes (see chain plan §C "Inputs"); we both **regenerate** them
//! from the recipes and **assert against the explicit hex bytes** in
//! the chain plan, so any discrepancy with the chain's BLAKE3 hashing
//! lands here and not in production.

use sum_store::assignment_v2::assigned_archives;
use sumchain_wire::Address;
use sumchain_wire::Hash;
use sumchain_wire::storage_metadata::assignment_score;

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

fn hex_to_arr32(h: &str) -> [u8; 32] {
    let v = hex::decode(h).expect("valid hex");
    assert_eq!(v.len(), 32);
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    a
}

fn hex_to_arr20(h: &str) -> [u8; 20] {
    let v = hex::decode(h).expect("valid hex");
    assert_eq!(v.len(), 20);
    let mut a = [0u8; 20];
    a.copy_from_slice(&v);
    a
}

// ── Tier 1: regenerated inputs match the explicit hex ─────────────────────────

#[test]
fn appendix_c_inputs_match_explicit_hex() {
    // Per Appendix C "Inputs" — verify our recipe-based construction
    // produces the exact hex strings the chain plan publishes.
    let r0 = hex_to_arr32("a5e2668f5022b62b5e4a1342aa0cfbfcbde2af2e3626b2fd57d6cf44e8f615a4");
    let r1 = hex_to_arr32("eed453d08260268bbd3675997f407174d901d842711f3addb6a2e05f776bccce");
    let r2 = hex_to_arr32("81137f39ea2a36bae5333d021052c44c0fc4763769c9988241e6669af16dfa74");
    assert_eq!(merkle_root(0), r0, "merkle_root[0] regen ≠ Appendix C hex");
    assert_eq!(merkle_root(1), r1, "merkle_root[1] regen ≠ Appendix C hex");
    assert_eq!(merkle_root(2), r2, "merkle_root[2] regen ≠ Appendix C hex");

    let a0 = hex_to_arr20("37c4401960bd5a26d8ed7b676b1ef47c78fac5bb");
    let a1 = hex_to_arr20("f1a469857483cc381865df996b2cccd254878a16");
    let a2 = hex_to_arr20("8c6a62e786d02ae255a6f481580b95fe05bafffc");
    let a3 = hex_to_arr20("f8967230e6a6d6b5b4ce6816d43f406f24d3cdad");
    let a4 = hex_to_arr20("7e65c99f5b3994f2014187f24ee9230a027526bd");
    assert_eq!(archive(0), a0, "archive[0] regen ≠ Appendix C hex");
    assert_eq!(archive(1), a1, "archive[1] regen ≠ Appendix C hex");
    assert_eq!(archive(2), a2, "archive[2] regen ≠ Appendix C hex");
    assert_eq!(archive(3), a3, "archive[3] regen ≠ Appendix C hex");
    assert_eq!(archive(4), a4, "archive[4] regen ≠ Appendix C hex");
}

// ── Tier 2: per-archive scores for (root[0], chunk_index=0) ───────────────────

#[test]
fn appendix_c_per_archive_scores_for_root0_chunk0() {
    let r0 = merkle_root(0);

    // Order from Appendix C "Per-archive scores" table — ascending by score.
    let cases: &[(usize, u64)] = &[
        (4, 0x4cd8130d5f5c7f55),
        (2, 0x73e9ad5ef9a6ba04),
        (1, 0xc8859dade38f7649),
        (3, 0xd2823bf6a2d883bb),
        (0, 0xf3c350979cb3f293),
    ];

    for &(j, expected) in cases {
        // The scorer now lives ONLY in the pinned sumchain-wire crate; SNIP
        // holds no copy. Convert SNIP's raw `[u8; 32]` / `[u8; 20]` into the
        // wire `Hash` / `Address` and score through the public entry point.
        let actual = assignment_score(&Hash::new(r0), 0, &Address::new(archive(j)));
        assert_eq!(
            actual, expected,
            "assignment_score(root[0], chunk_index=0, archive[{j}]) = {actual:#018x}, \
             expected {expected:#018x}\n\
             The scorer is delegated to sumchain-wire's storage_metadata::assignment_score;\n\
             a mismatch means the pinned wire crate diverged from chain plan Appendix C\n\
             (unexpected for the frozen =0.3.0 pin) — verify the pin and the Appendix-C inputs."
        );
    }
}

// ── Tier 3: assignment outputs ────────────────────────────────────────────────

#[test]
fn appendix_c_assignment_outputs() {
    // Pre-build the snapshot once. Note the chain plan emphasizes "any
    // order — assignment fn sorts internally", so we deliberately pass
    // the archives in the order Appendix C lists, NOT sorted by address.
    let snapshot = vec![archive(0), archive(1), archive(2), archive(3), archive(4)];

    // Each row from Appendix C: (file_idx, chunk_index, R, expected indices into archive())
    let cases: &[(usize, u32, u32, &[usize])] = &[
        (0, 0, 1, &[4]),
        (0, 0, 3, &[4, 2, 1]),
        (0, 7, 3, &[3, 0, 4]),
        (1, 0, 3, &[1, 2, 0]),
        (1, 1, 3, &[4, 2, 3]),
        (2, 42, 3, &[1, 2, 3]),
        (2, 42, 5, &[1, 2, 3, 0, 4]),
        // R=7 must clamp to snapshot.len()=5 and produce identical output to R=5.
        (2, 42, 7, &[1, 2, 3, 0, 4]),
    ];

    for &(file_idx, chunk_index, r, expected_archive_indices) in cases {
        let root = merkle_root(file_idx);
        let actual = assigned_archives(&root, &snapshot, chunk_index, r);
        let expected: Vec<[u8; 20]> = expected_archive_indices
            .iter()
            .map(|&j| archive(j))
            .collect();
        assert_eq!(
            actual, expected,
            "assignment(file={file_idx}, chunk_index={chunk_index}, R={r}) diverges from Appendix C"
        );
    }
}

// ── Tier 4: smoke check on order-invariance ───────────────────────────────────

#[test]
fn snapshot_input_order_does_not_affect_output() {
    // The chain plan says "any order — assignment fn sorts internally".
    // Verify by running the same query with a shuffled snapshot.
    let snapshot_a = vec![archive(0), archive(1), archive(2), archive(3), archive(4)];
    let snapshot_b = vec![archive(3), archive(0), archive(4), archive(2), archive(1)];
    let r0 = merkle_root(0);

    for chunk_index in [0u32, 7, 42, 1000, u32::MAX] {
        for r in [1u32, 3, 5] {
            assert_eq!(
                assigned_archives(&r0, &snapshot_a, chunk_index, r),
                assigned_archives(&r0, &snapshot_b, chunk_index, r),
                "order-invariance broken at chunk_index={chunk_index}, R={r}"
            );
        }
    }
}
