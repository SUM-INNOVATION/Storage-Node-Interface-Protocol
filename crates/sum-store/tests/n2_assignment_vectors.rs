//! SNIP's V2 assignment against vectors the CHAIN produced.
//!
//! `tests/data/n2_assignment_vectors/n2_assignment_vectors.json` was written by
//! `generate_in_sum_chain.rs` (same directory) running inside sum-chain at
//! `d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2`, against the real executor and
//! the committed coverage reader. For files at and above the coverage RPC's
//! 16,384-chunk `assigned_count` cap it records, for the latest epoch:
//!
//! * the RPC's own `assigned_count` per archive (`null` above the cap);
//! * the count under `AcceptAssignmentV2`'s validity rule;
//! * a BLAKE3 digest of the whole per-chunk assignment;
//! * whether an archive that has left holds a chunk (the N2 condition).
//!
//! This test recomputes all of it with `sum_store::assignment_v2` (the
//! published `sumchain-wire` scorer SNIP links) and requires exact equality,
//! so SNIP's local computation above the cap is checked against the chain's,
//! not only against itself.

use std::path::PathBuf;

use serde::Deserialize;
use sum_store::assignment_v2::{
    chunks_for_archive_v2, compute_assignment_v2, first_chunk_assigned_to_any,
};
use sumchain_wire::{Address, Hash};

#[derive(Deserialize)]
struct Doc {
    provenance: Provenance,
    replication_factor: u32,
    max_assigned_count_chunk_count: u32,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Provenance {
    sum_chain_commit: String,
}

#[derive(Deserialize)]
struct Case {
    label: String,
    merkle_root: String,
    chunk_count: u32,
    assignment_epochs: Vec<u64>,
    latest_epoch_height: u64,
    reassignment_needed: bool,
    latest_per_archive: Vec<Entry>,
    assignment_digest_blake3: String,
    departed_assignee: bool,
}

#[derive(Deserialize)]
struct Entry {
    archive: String,
    address_hex: String,
    currently_active: bool,
    rpc_assigned_count: Option<u32>,
    executor_assigned_count: u32,
}

fn arr<const N: usize>(hex_str: &str) -> [u8; N] {
    hex::decode(hex_str)
        .expect("hex")
        .try_into()
        .expect("length")
}

fn load() -> Doc {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/n2_assignment_vectors/n2_assignment_vectors.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("vectors file")).expect("json")
}

#[test]
fn vectors_come_from_the_expected_chain_commit_and_cover_the_cap() {
    let doc = load();
    assert_eq!(
        doc.provenance.sum_chain_commit,
        "d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2"
    );
    assert_eq!(doc.max_assigned_count_chunk_count, 16_384);
    let cap = doc.max_assigned_count_chunk_count;
    assert!(doc.cases.iter().any(|c| c.chunk_count == cap));
    assert!(doc.cases.iter().any(|c| c.chunk_count == cap + 1));
    assert!(doc.cases.iter().any(|c| c.chunk_count > cap + 1));
    assert!(doc.cases.iter().any(|c| !c.departed_assignee));
    assert!(
        doc.cases
            .iter()
            .any(|c| c.chunk_count > cap && c.assignment_epochs.len() > 1),
        "an above-cap file whose latest epoch is a reassignment"
    );
    for c in &doc.cases {
        assert!(c.reassignment_needed, "{}", c.label);
        assert_eq!(c.assignment_epochs.last(), Some(&c.latest_epoch_height));
        for e in &c.latest_per_archive {
            assert_eq!(
                e.rpc_assigned_count.is_some(),
                c.chunk_count <= cap,
                "{}: the RPC elides counts exactly above the cap",
                c.label
            );
        }
    }
}

#[test]
fn local_assignment_reproduces_the_chains_vectors_exactly() {
    let doc = load();
    let r = doc.replication_factor;
    for c in &doc.cases {
        let root: [u8; 32] = arr(&c.merkle_root);
        let snapshot: Vec<[u8; 20]> = c
            .latest_per_archive
            .iter()
            .map(|e| {
                let a: [u8; 20] = arr(&e.address_hex);
                assert_eq!(
                    Address::from_base58(&e.archive).expect("base58").as_bytes(),
                    &a,
                    "{}: base58 and hex name the same archive",
                    c.label
                );
                a
            })
            .collect();

        // Every chunk, in order: the chain's per-chunk assignment digest.
        let mut input = Vec::with_capacity(c.chunk_count as usize * r as usize * 20);
        for chunk in compute_assignment_v2(&root, c.chunk_count, &snapshot, r) {
            for a in chunk {
                input.extend_from_slice(&a);
            }
        }
        assert_eq!(
            hex::encode(Hash::hash(&input).as_bytes()),
            c.assignment_digest_blake3,
            "{}: per-chunk assignment digest",
            c.label
        );

        // Per-archive counts: the validity rule's, and the RPC's where given.
        for e in &c.latest_per_archive {
            let a: [u8; 20] = arr(&e.address_hex);
            let n = chunks_for_archive_v2(&root, c.chunk_count, &snapshot, r, &a).len() as u32;
            assert_eq!(n, e.executor_assigned_count, "{}: {}", c.label, e.archive);
            if let Some(rpc) = e.rpc_assigned_count {
                assert_eq!(n, rpc, "{}: {} RPC count", c.label, e.archive);
            }
        }

        // The N2 condition, by the early-exit search the reassign tool uses.
        let departed: Vec<[u8; 20]> = c
            .latest_per_archive
            .iter()
            .filter(|e| !e.currently_active)
            .map(|e| arr(&e.address_hex))
            .collect();
        assert!(!departed.is_empty(), "{}", c.label);
        assert_eq!(
            first_chunk_assigned_to_any(&root, c.chunk_count, &snapshot, r, &departed).is_some(),
            c.departed_assignee,
            "{}: departed assignee",
            c.label
        );
    }
}
