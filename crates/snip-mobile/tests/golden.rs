//! Golden vectors for the consensus-critical helpers exposed over FFI.
//!
//! The fixture at tests/golden/vectors.json is consumed byte-identically
//! by the wallet's Swift test suite (through the same FFI functions), so
//! a change in tree shape or assignment scoring fails BOTH suites.
//!
//! Regenerate deliberately with: UPDATE_GOLDEN=1 cargo test -p snip-mobile

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use snip_mobile::{compute_chunk_assignment, compute_merkle_root};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/vectors.json");

/// 3.5 MiB deterministic pattern → 4 chunks (3 full + 1 partial).
fn write_test_file(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("golden-input.bin");
    let mut f = fs::File::create(&path).unwrap();
    let len = 3 * 1024 * 1024 + 512 * 1024;
    let block: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let mut written = 0usize;
    while written < len {
        let take = block.len().min(len - written);
        f.write_all(&block[..take]).unwrap();
        written += take;
    }
    path
}

/// Synthetic 7-archive snapshot: addresses [0x11*i; 20], base58-encoded.
fn synthetic_nodes() -> Vec<String> {
    (1u8..=7)
        .map(|i| sum_net::l1_address_base58(&[i * 0x11; 20]))
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Vectors {
    file_len: u64,
    merkle_root_hex: String,
    node_addresses: Vec<String>,
    /// assignments[i] = top-3 archives for chunk i
    assignments: Vec<Vec<String>>,
}

fn compute_vectors() -> Vectors {
    let dir = tempfile::tempdir().unwrap();
    let path = write_test_file(&dir);
    let root = compute_merkle_root(path.to_string_lossy().into_owned()).unwrap();
    let nodes = synthetic_nodes();
    let assignments = (0..4u32)
        .map(|i| compute_chunk_assignment(root.clone(), i, nodes.clone(), 3).unwrap())
        .collect();
    Vectors {
        file_len: (3 * 1024 * 1024 + 512 * 1024) as u64,
        merkle_root_hex: root,
        node_addresses: nodes,
        assignments,
    }
}

#[test]
fn golden_vectors_are_stable() {
    let computed = compute_vectors();

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        fs::create_dir_all(PathBuf::from(FIXTURE).parent().unwrap()).unwrap();
        fs::write(FIXTURE, serde_json::to_string_pretty(&computed).unwrap()).unwrap();
        return;
    }

    let stored: Vectors = serde_json::from_str(
        &fs::read_to_string(FIXTURE)
            .expect("fixture missing — run once with UPDATE_GOLDEN=1"),
    )
    .unwrap();
    assert_eq!(
        computed, stored,
        "merkle tree shape or assignment scoring changed — this breaks \
         consensus with deployed archives; if intentional, bump the \
         protocol context string and regenerate"
    );
}

#[test]
fn assignment_is_order_insensitive() {
    let v = compute_vectors();
    let mut shuffled = v.node_addresses.clone();
    shuffled.reverse();
    for (i, expected) in v.assignments.iter().enumerate() {
        let got =
            compute_chunk_assignment(v.merkle_root_hex.clone(), i as u32, shuffled.clone(), 3)
                .unwrap();
        assert_eq!(&got, expected, "chunk {i}: snapshot order must not matter");
    }
}

#[test]
fn replication_factor_clamps_to_snapshot() {
    let v = compute_vectors();
    let two_nodes = v.node_addresses[..2].to_vec();
    let got = compute_chunk_assignment(v.merkle_root_hex.clone(), 0, two_nodes, 3).unwrap();
    assert_eq!(got.len(), 2);
}
