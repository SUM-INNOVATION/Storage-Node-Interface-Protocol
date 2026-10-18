//! GENERATOR for SNIP's N2 assignment vectors (`n2_assignment_vectors.json`
//! next to this file). It is kept here as provenance and is NOT compiled by
//! SNIP: cargo only builds `tests/*.rs`, not files under `tests/data/`.
//!
//! It runs inside sum-chain, against the chain's own code:
//!
//! ```text
//! git -C sum-chain checkout d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2
//! cp generate_in_sum_chain.rs sum-chain/crates/state/tests/snip_n2_assignment_vectors.rs
//! SNIP_N2_VECTORS_OUT=/abs/path/n2_assignment_vectors.json \
//!   cargo test -p sumchain-state --test snip_n2_assignment_vectors -- --nocapture
//! ```
//!
//! What it does:
//!
//! * Drives a temp chain block by block through the REAL executor: seven
//!   archives with fixed keys register, five files are registered with fixed
//!   roots and chunk counts on both sides of the coverage RPC's 16,384-chunk
//!   `assigned_count` cap, one archive leaves by its own unstake, one large file
//!   is reassigned, and a second archive then leaves.
//! * Reads each file's coverage with `StorageMetadataExecutor::compute_coverage_v2`,
//!   the committed reader behind `storage_getAssignmentCoverageV2`. That gives
//!   the chain's own `assigned_count` (or `None` above the cap),
//!   `currently_active` and `reassignment_needed`.
//! * Recomputes every archive's count in the latest epoch with
//!   `is_archive_assigned_to_chunk`, the function `AcceptAssignmentV2` validity
//!   uses, over the snapshot from `get_active_archive_nodes_at_height`, and
//!   asserts it equals the RPC count wherever the RPC gives one.
//! * Records a BLAKE3 digest of the whole per-chunk assignment (for each chunk
//!   in order, the 20-byte addresses `assigned_archives_presorted` returns, in
//!   its order) so a consumer can check its computation chunk by chunk, not
//!   only in aggregate.
//! * Records whether an archive that has left the active set holds at least one
//!   chunk of the file in the latest epoch: the N2 condition.

mod common;
use common::{fund, publish_block, setup_with_params, CHAIN_ID};

use std::sync::Arc;

use sumchain_crypto::{sign, KeyPair};
use sumchain_genesis::ChainParams;
use sumchain_primitives::{
    assigned_archives, assigned_archives_presorted, is_archive_assigned_to_chunk, Address, Hash,
    NodeRegistryOperation, NodeRegistryTxData, NodeRole, SignedTransaction,
    StorageMetadataOperationV2, StorageMetadataV2TxData, TransactionV2, TxPayload,
};
use sumchain_state::executor::BlockExecutor;
use sumchain_state::state::StateManager;
use sumchain_state::storage_metadata::{StorageMetadataExecutor, MAX_ASSIGNED_COUNT_CHUNK_COUNT};
use sumchain_state::NodeRegistryExecutor;
use sumchain_storage::Database;

const STAKE: u64 = 1_000_000_000;
const FEE: u128 = 1_000;
const FEE_DEPOSIT: u64 = 1_000_000;
const CHUNK_SIZE: u64 = 1_048_576;
/// Mainnet's replication factor.
const R: u32 = 3;
const ARCHIVES: u8 = 7;

/// (label, chunk_count). Two at or below the 16,384 cap, three above it.
const FILES: [(&str, u32); 5] = [
    ("one-chunk", 1),
    ("small-37", 37),
    ("at-cap-16384", 16_384),
    ("above-cap-16385", 16_385),
    ("large-40000", 40_000),
];
/// The above-cap file that is reassigned after the first departure.
const REASSIGNED: &str = "large-40000";

fn params() -> ChainParams {
    let mut p = ChainParams::with_v2_enabled();
    p.archive_reassignment_enabled_from_height = Some(0);
    p.archive_unbonding_enabled_from_height = Some(0);
    p.assignment_replication_factor = R;
    p
}

fn signed(kp: &KeyPair, nonce: u64, payload: TxPayload) -> SignedTransaction {
    let tx = TransactionV2 {
        chain_id: CHAIN_ID,
        from: kp.address(),
        fee: FEE,
        nonce,
        payload,
    };
    let h = tx.signing_hash();
    let sig = sign(h.as_bytes(), kp.private_key());
    SignedTransaction::new_v2(tx, *sig.as_bytes(), *kp.public_key().as_bytes())
}

fn nr(op: NodeRegistryOperation) -> TxPayload {
    TxPayload::NodeRegistry(NodeRegistryTxData { operation: op })
}

fn sm(op: StorageMetadataOperationV2) -> TxPayload {
    TxPayload::StorageMetadataV2(StorageMetadataV2TxData { operation: op })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn root_of(label: &str) -> Hash {
    Hash::hash(format!("snip-n2-assignment-vector/{label}").as_bytes())
}

struct Chain {
    state: Arc<StateManager>,
    db: Arc<Database>,
    executor: BlockExecutor,
    _dir: tempfile::TempDir,
    proposer: KeyPair,
    owner: KeyPair,
    owner_nonce: u64,
    archives: Vec<(KeyPair, u64)>,
}

impl Chain {
    fn publish_ok(&self, h: u64, txs: Vec<SignedTransaction>) {
        let pk = *self.proposer.public_key().as_bytes();
        for r in publish_block(&self.state, &self.executor, h, &pk, txs, &[]) {
            assert!(r.status.is_success(), "height {h}: {:?}", r.status);
        }
    }

    fn owner_tx(&mut self, payload: TxPayload) -> SignedTransaction {
        let tx = signed(&self.owner, self.owner_nonce, payload);
        self.owner_nonce += 1;
        tx
    }

    fn leave(&mut self, h: u64, who: Address) {
        let entry = self
            .archives
            .iter_mut()
            .find(|(kp, _)| kp.address() == who)
            .expect("a registered archive");
        let tx = signed(
            &entry.0,
            entry.1,
            nr(NodeRegistryOperation::BeginUnstake { amount: STAKE }),
        );
        entry.1 += 1;
        self.publish_ok(h, vec![tx]);
    }

    fn snapshot_at(&self, epoch: u64) -> Vec<Address> {
        NodeRegistryExecutor::new(self.db.clone())
            .get_active_archive_nodes_at_height(epoch)
            .unwrap()
            .iter()
            .map(|n| n.address)
            .collect()
    }
}

#[test]
fn generate_snip_n2_assignment_vectors() {
    let out_path = std::env::var("SNIP_N2_VECTORS_OUT")
        .expect("set SNIP_N2_VECTORS_OUT to the absolute output path");
    assert_eq!(MAX_ASSIGNED_COUNT_CHUNK_COUNT, 16_384, "the cap this generator assumes");

    let (state, db, dir, executor) = setup_with_params(params());
    // Fixed keys so the archive addresses, and therefore the vectors, are
    // reproducible. These are test keys only.
    let archives: Vec<(KeyPair, u64)> = (0..ARCHIVES)
        .map(|i| (KeyPair::from_bytes([0xA0 + i; 32]), 0))
        .collect();
    for (a, _) in &archives {
        fund(&db, a, (STAKE as u128) + 1_000_000);
    }
    let owner = KeyPair::from_bytes([0x0E; 32]);
    fund(&db, &owner, 1_000_000_000_000);
    let mut ch = Chain {
        state,
        db,
        executor,
        _dir: dir,
        proposer: KeyPair::from_bytes([0x0F; 32]),
        owner,
        owner_nonce: 0,
        archives,
    };

    // h1: every archive registers.
    let regs: Vec<SignedTransaction> = ch
        .archives
        .iter()
        .map(|(a, _)| {
            signed(
                a,
                0,
                nr(NodeRegistryOperation::Register {
                    role: NodeRole::ArchiveNode,
                    stake: STAKE,
                }),
            )
        })
        .collect();
    ch.publish_ok(1, regs);
    for entry in &mut ch.archives {
        entry.1 = 1;
    }

    // h2: every file registers (epoch 0 = 2 for all of them).
    let mut txs = Vec::new();
    for (label, chunk_count) in FILES {
        txs.push(ch.owner_tx(sm(StorageMetadataOperationV2::RegisterFilePendingV2 {
            merkle_root: root_of(label),
            plaintext_size_bytes: chunk_count as u64 * CHUNK_SIZE,
            stored_size_bytes: chunk_count as u64 * CHUNK_SIZE,
            chunk_count,
            fee_deposit: FEE_DEPOSIT,
            visibility: 0,
            initial_access: vec![],
        })));
    }
    ch.publish_ok(2, txs);
    let snapshot0 = ch.snapshot_at(2);
    assert_eq!(snapshot0.len(), ARCHIVES as usize);

    // The first archive to leave holds NO chunk of the one-chunk file, so that
    // file gives the "flag set, no departed assignee" case at or below the cap.
    let one_chunk_assignees = assigned_archives(&root_of("one-chunk"), &snapshot0, 0, R);
    let first_leaver = *snapshot0
        .iter()
        .find(|a| !one_chunk_assignees.contains(a))
        .expect("seven archives, three assignees");

    // h3: it leaves. h4: the large file is reassigned (its new snapshot no
    // longer contains the leaver). h5: a second archive leaves, one that is in
    // the new snapshot and, again, holds nothing of the one-chunk file.
    ch.leave(3, first_leaver);
    let re = ch.owner_tx(sm(StorageMetadataOperationV2::ReassignChunksV2 {
        merkle_root: root_of(REASSIGNED),
    }));
    ch.publish_ok(4, vec![re]);
    let snapshot4 = ch.snapshot_at(4);
    assert!(!snapshot4.contains(&first_leaver));
    let second_leaver = *snapshot4
        .iter()
        .find(|a| !one_chunk_assignees.contains(a))
        .expect("six archives, three one-chunk assignees");
    ch.leave(5, second_leaver);

    let storage = StorageMetadataExecutor::new(ch.db.clone());
    let registry = NodeRegistryExecutor::new(ch.db.clone());
    let mut cases = Vec::new();
    for (label, chunk_count) in FILES {
        let root = root_of(label);
        let cov = storage
            .compute_coverage_v2(&root, &registry, R)
            .unwrap()
            .expect("file registered");
        assert!(cov.reassignment_needed, "{label}: a snapshot archive has left");
        let latest = cov.per_epoch.last().expect("epoch 0 always present");
        assert_eq!(latest.epoch_height, cov.latest_assignment_epoch);
        let snapshot = ch.snapshot_at(latest.epoch_height);

        let mut sorted = snapshot.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        sorted.dedup_by(|a, b| a.as_bytes() == b.as_bytes());

        // Executor-rule counts and the per-chunk digest.
        let mut executor_counts = vec![0u32; latest.per_archive.len()];
        let mut digest_input: Vec<u8> = Vec::with_capacity(chunk_count as usize * R as usize * 20);
        for idx in 0..chunk_count {
            for a in assigned_archives_presorted(&root, &sorted, idx, R) {
                digest_input.extend_from_slice(a.as_bytes());
            }
            for (i, p) in latest.per_archive.iter().enumerate() {
                if is_archive_assigned_to_chunk(&root, &snapshot, idx, R, &p.archive) {
                    executor_counts[i] += 1;
                }
            }
        }

        let mut per_archive = Vec::new();
        let mut departed_assignee = false;
        for (i, p) in latest.per_archive.iter().enumerate() {
            if let Some(n) = p.assigned_count {
                assert_eq!(
                    n, executor_counts[i],
                    "{label}: RPC count and executor rule disagree"
                );
            }
            assert_eq!(
                p.assigned_count.is_some(),
                chunk_count <= MAX_ASSIGNED_COUNT_CHUNK_COUNT,
                "{label}: the cap"
            );
            if !p.currently_active && executor_counts[i] > 0 {
                departed_assignee = true;
            }
            per_archive.push(serde_json::json!({
                "archive": p.archive.to_base58(),
                "address_hex": hex(p.archive.as_bytes()),
                "currently_active": p.currently_active,
                "rpc_assigned_count": p.assigned_count,
                "executor_assigned_count": executor_counts[i],
            }));
        }
        cases.push(serde_json::json!({
            "label": label,
            "merkle_root": hex(root.as_bytes()),
            "chunk_count": chunk_count,
            "assignment_epochs": cov.assignment_epochs,
            "latest_epoch_height": latest.epoch_height,
            "reassignment_needed": cov.reassignment_needed,
            "latest_per_archive": per_archive,
            "assignment_digest_blake3": hex(Hash::hash(&digest_input).as_bytes()),
            "departed_assignee": departed_assignee,
        }));
    }

    let doc = serde_json::json!({
        "provenance": {
            "sum_chain_commit": "d8130ead3b77f38eff6eeb82b4e68bb1aca8e3b2",
            "generator": "crates/sum-store/tests/data/n2_assignment_vectors/generate_in_sum_chain.rs, copied to sum-chain crates/state/tests/snip_n2_assignment_vectors.rs",
            "command": "SNIP_N2_VECTORS_OUT=<path> cargo test -p sumchain-state --test snip_n2_assignment_vectors -- --nocapture",
            "chain_code_paths": [
                "StorageMetadataExecutor::compute_coverage_v2 (committed reader behind storage_getAssignmentCoverageV2)",
                "sumchain_primitives::is_archive_assigned_to_chunk (AcceptAssignmentV2 validity)",
                "sumchain_primitives::assigned_archives_presorted (per-chunk digest)",
                "NodeRegistryExecutor::get_active_archive_nodes_at_height (epoch snapshot)"
            ],
            "keys": "fixed test keys [0xA0+i; 32] (archives), [0x0E; 32] (owner); test-only"
        },
        "replication_factor": R,
        "max_assigned_count_chunk_count": MAX_ASSIGNED_COUNT_CHUNK_COUNT,
        "digest": "BLAKE3 over, for each chunk index ascending, the 20-byte addresses assigned_archives_presorted returns for it, in its order",
        "cases": cases,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    println!("wrote {out_path}");
}
