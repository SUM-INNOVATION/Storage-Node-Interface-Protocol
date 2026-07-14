//! Shared fixtures + helpers for the WS2a mock-driven Phase 4 lifecycle
//! suite. None of these helpers represent real signing material, real
//! funded accounts, or real chain state.
//!
//! Constants in this module that LOOK like seeds or keys are deliberately
//! constant byte patterns (`[0x42; 32]`, `[0xCD; 32]`, etc.) chosen for
//! determinism in tests. They are NOT funded. They are NOT registered on
//! any chain. They are NOT secrets — they're test fixtures.
//!
//! Production keys live outside the repo per `docs/PRIVACY-AUDIT.md`
//! and `docs/OPERATOR-RUNBOOK.md`.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;

use sum_crypto::{
    RECIPIENT_BUNDLE_SIZE, encrypt_chunk, encrypt_manifest, wrap_for_recipient,
    x25519_keypair_from_ed25519_seed,
};
use sum_node::download_private::AccessListSource;
use sum_store::merkle::MerkleTree;
use sum_types::rpc_types::{AccessEntryV2, LifecycleV2, StorageFileInfoV2, VisibilityV2};
use sum_types::storage::{ChunkDescriptor, DataManifest};
use zeroize::Zeroizing;

// ── Test seeds and addresses ────────────────────────────────────────────────
//
// These byte patterns are chosen for determinism in tests. They are NOT
// real Ed25519 seeds in any operational sense — never copy these into a
// running node, never use them to sign anything against a chain you care
// about.

/// Owner test seed. Constant byte pattern.
pub const OWNER_SEED: [u8; 32] = [0xAA; 32];

/// Recipient test seed. Constant byte pattern.
pub const RECIPIENT_SEED: [u8; 32] = [0xBB; 32];

/// Third-party recipient seed (used by share-simulation tests).
pub const THIRD_PARTY_SEED: [u8; 32] = [0xCC; 32];

/// Owner test L1 address (20 bytes). NOT a real address; pure test fixture.
pub const OWNER_ADDR: [u8; 20] = [0x11; 20];

/// Recipient test L1 address.
pub const RECIPIENT_ADDR: [u8; 20] = [0x22; 20];

/// Third-party test L1 address.
pub const THIRD_PARTY_ADDR: [u8; 20] = [0x33; 20];

/// Render a 20-byte test address as base58 (matches the chain's
/// access-list address format).
pub fn addr_b58(addr: &[u8; 20]) -> String {
    sum_net::identity::l1_address_base58(addr)
}

/// Derive the X25519 recipient pubkey for a test seed. Matches what
/// the chain's `account_getEncryptionPublicKey` would return after
/// `RegisterEncryptionKey`.
pub fn x25519_pub_for_seed(seed: &[u8; 32]) -> [u8; 32] {
    let (_sk, pk) = x25519_keypair_from_ed25519_seed(seed);
    pk
}

/// Derive an X25519 secret (zeroizing) for a test seed. The download
/// path takes this in `Zeroizing<[u8; 32]>` form.
pub fn x25519_secret_for_seed(seed: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let (sk, _pk) = x25519_keypair_from_ed25519_seed(seed);
    Zeroizing::new(sk)
}

// ── K_file fixtures ─────────────────────────────────────────────────────────

/// Deterministic test K_file. Production K_file is generated via
/// `OsRng` per file ingest; tests bypass that to get deterministic
/// merkle roots. Real chunks encrypted with this K_file are NOT
/// safe to ship — it's a fixed pattern.
pub fn make_test_kfile() -> Zeroizing<[u8; 32]> {
    Zeroizing::new([0x42u8; 32])
}

// ── Plaintext fixtures ──────────────────────────────────────────────────────

/// A small deterministic test plaintext. 256 bytes of `(i & 0xff)`.
/// Just big enough to chunk into one chunk under the chain's default
/// chunk-size cap; tests don't exercise multi-chunk chunking here.
pub fn small_plaintext() -> Vec<u8> {
    (0..256).map(|i| (i & 0xff) as u8).collect()
}

// ── Encrypted-artifact builder ──────────────────────────────────────────────

/// Output of [`build_private_test_artifacts`]: everything a recipient
/// would need from "the chain + the wire" to decrypt and verify a
/// Private file.
pub struct PrivateTestArtifacts {
    /// The chain root (= manifest's merkle_root). Matches what
    /// `RegisterFilePendingV2` would carry on chain.
    pub merkle_root: [u8; 32],
    /// Per-chunk ciphertext blobs in chunk_index order.
    pub ciphertext_chunks: Vec<Vec<u8>>,
    /// Encrypted manifest blob (CBOR plaintext → ChaCha20-Poly1305).
    pub encrypted_manifest_bytes: Vec<u8>,
    /// Plaintext manifest used to seed the encrypted form. Tests
    /// usually don't read this; included so assertions can compare
    /// against the original.
    pub manifest: DataManifest,
}

/// Encrypt a plaintext under a known K_file and return everything a
/// recipient would need: ciphertext chunks, encrypted manifest, and
/// the chain root. The recipient (any party with K_file) can drive
/// `decrypt_and_verify_manifest` + `decrypt_and_verify_chunk` with
/// these outputs to round-trip the plaintext.
///
/// One-chunk only by design — the suite is testing the lifecycle
/// contract, not the chunker; see `sum_store::chunker` tests for
/// multi-chunk coverage.
pub fn build_private_test_artifacts(
    plaintext: &[u8],
    k_file: &Zeroizing<[u8; 32]>,
) -> PrivateTestArtifacts {
    let pt_hash = *blake3::hash(plaintext).as_bytes();
    let ciphertext = encrypt_chunk(k_file, 0, plaintext);
    let ct_hash = *blake3::hash(&ciphertext).as_bytes();
    let cid = sum_store::cid_from_data(&ciphertext);

    let chunk = ChunkDescriptor {
        chunk_index: 0,
        offset: 0,
        size: ciphertext.len() as u64,
        blake3_hash: ct_hash,
        cid,
        plaintext_blake3_hash: Some(pt_hash),
    };
    let leaves = vec![blake3::Hash::from(ct_hash)];
    let merkle_root = *MerkleTree::build(&leaves).root().as_bytes();
    let file_hash = *blake3::hash(plaintext).as_bytes();
    let manifest = DataManifest {
        file_name: "private-test-fixture.bin".into(),
        file_hash,
        total_size_bytes: plaintext.len() as u64,
        chunk_count: 1,
        merkle_root,
        chunks: vec![chunk],
    };
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(&manifest, &mut cbor).expect("CBOR encode");
    let encrypted_manifest_bytes = encrypt_manifest(k_file, &cbor);
    PrivateTestArtifacts {
        merkle_root,
        ciphertext_chunks: vec![ciphertext],
        encrypted_manifest_bytes,
        manifest,
    }
}

// ── Public-file artifact builder ────────────────────────────────────────────

/// Public version of [`build_private_test_artifacts`]: no K_file,
/// no encryption. Returns the chunk bytes + plaintext manifest +
/// chain root.
pub struct PublicTestArtifacts {
    pub merkle_root: [u8; 32],
    pub chunks: Vec<Vec<u8>>,
    pub manifest: DataManifest,
}

pub fn build_public_test_artifacts(plaintext: &[u8]) -> PublicTestArtifacts {
    let chunk_hash = *blake3::hash(plaintext).as_bytes();
    let cid = sum_store::cid_from_data(plaintext);
    let chunk = ChunkDescriptor {
        chunk_index: 0,
        offset: 0,
        size: plaintext.len() as u64,
        blake3_hash: chunk_hash,
        cid,
        plaintext_blake3_hash: None,
    };
    let leaves = vec![blake3::Hash::from(chunk_hash)];
    let merkle_root = *MerkleTree::build(&leaves).root().as_bytes();
    let file_hash = *blake3::hash(plaintext).as_bytes();
    let manifest = DataManifest {
        file_name: "public-test-fixture.bin".into(),
        file_hash,
        total_size_bytes: plaintext.len() as u64,
        chunk_count: 1,
        merkle_root,
        chunks: vec![chunk],
    };
    PublicTestArtifacts {
        merkle_root,
        chunks: vec![plaintext.to_vec()],
        manifest,
    }
}

// ── Access-entry builder ────────────────────────────────────────────────────

/// Build a chain-shaped `AccessEntryV2` for `recipient_addr`,
/// wrapping `k_file` for that recipient's X25519 pubkey. Returns the
/// entry ready to be inserted into a `StorageFileInfoV2.access_list`.
pub fn make_access_entry(
    recipient_addr: &[u8; 20],
    recipient_x25519_pub: &[u8; 32],
    k_file: &[u8; 32],
    expires_at: Option<u64>,
) -> AccessEntryV2 {
    let bundle = wrap_for_recipient(k_file, recipient_addr, recipient_x25519_pub)
        .expect("wrap_for_recipient");
    let bundle_hex = format!("0x{}", hex::encode(bundle));
    AccessEntryV2 {
        address: addr_b58(recipient_addr),
        encrypted_key_bundle: Some(bundle_hex),
        expires_at,
    }
}

// ── StorageFileInfoV2 builder ───────────────────────────────────────────────

/// Build a chain-shaped Active Private V2 row with the supplied
/// access list. Other fields use sensible defaults — none of them
/// are load-bearing for the lifecycle tests in this suite.
pub fn private_active_file_info(
    merkle_root: [u8; 32],
    owner_addr: &[u8; 20],
    chunk_count: u32,
    access_list: Vec<AccessEntryV2>,
) -> StorageFileInfoV2 {
    StorageFileInfoV2 {
        merkle_root: format!("0x{}", hex::encode(merkle_root)),
        owner: addr_b58(owner_addr),
        plaintext_size_bytes: 0,
        stored_size_bytes: 0,
        chunk_count,
        fee_pool: 0,
        created_at: 0,
        activated_at_height: Some(100),
        abandoned_at_height: None,
        assignment_height: 0,
        visibility: VisibilityV2::PRIVATE,
        lifecycle: LifecycleV2::ACTIVE,
        access_list,
    }
}

/// Build a chain-shaped Active Public V2 row.
pub fn public_active_file_info(merkle_root: [u8; 32], chunk_count: u32) -> StorageFileInfoV2 {
    StorageFileInfoV2 {
        merkle_root: format!("0x{}", hex::encode(merkle_root)),
        owner: addr_b58(&OWNER_ADDR),
        plaintext_size_bytes: 0,
        stored_size_bytes: 0,
        chunk_count,
        fee_pool: 0,
        created_at: 0,
        activated_at_height: Some(100),
        abandoned_at_height: None,
        assignment_height: 0,
        visibility: VisibilityV2::PUBLIC,
        lifecycle: LifecycleV2::ACTIVE,
        access_list: Vec::new(),
    }
}

// ── AccessListSource fixture ────────────────────────────────────────────────
//
// Most lifecycle tests pass the StorageFileInfoV2 to
// `find_my_access_entry` directly (that fn takes a `first_page`
// param). When a test wants to exercise the pagination path or
// simulate a chain RPC error, this single-page mock is enough — the
// production `L1RpcClient` impl is fully covered by the pagination
// tests in `download_private.rs::tests`.

pub struct StaticAccessRpc {
    pub first_page: StorageFileInfoV2,
}

#[async_trait]
impl AccessListSource for StaticAccessRpc {
    async fn fetch_page(&self, _: &str, _: u32, _: u32) -> Result<Option<StorageFileInfoV2>> {
        // First-page short-circuit means this is rarely called. The
        // existing `find_my_access_entry_no_access_short_first_page`
        // test pins that branch.
        Ok(Some(self.first_page.clone()))
    }
}

// ── WS2b helpers (real local-mirror harness) ────────────────────────────────
//
// Helpers below are used by `tests/e2e_mirror.rs` (all `#[ignore]`'d).
// They assume:
//   * The chain mirror is already running at `http://localhost:8545`.
//   * `e2e_keys/` exists at the SNIP repo root (operator ran
//     `e2e-helper generate-e2e-keys --out e2e_keys` and brought up
//     the mirror with the corresponding `extra-alloc.json` overlay).
//
// Tests fail-fast with actionable messages if either assumption is
// violated — so an operator running `make e2e-mirror` against an
// un-funded mirror sees exactly what to fix, not a confusing
// libp2p / RPC error chain.

use std::path::Path as StdPath;

use sum_node::rpc_client::L1RpcClient;
use sum_types::rpc_types::TxStatusV2;

/// Default chain mirror RPC URL. Overrideable per-test via the
/// `SNIP_E2E_RPC_URL` env var.
pub const E2E_RPC_URL_DEFAULT: &str = "http://localhost:8545";

/// Wall-clock budget for a single tx to finalize. The mirror's
/// declared block cadence is ~2s; finality_depth=3 → expect ~6s in
/// the steady-state. Budget 16s to absorb CI runner jitter / cold
/// boot. WS2b is the wall-clock layer; if chain ever ships a
/// `dev_finalizeNow` RPC, swap this helper's body — the test
/// contract stays identical.
pub const E2E_FINALITY_BUDGET_SECS: u64 = 16;

/// Resolve the RPC URL the harness should target.
pub fn e2e_rpc_url() -> String {
    std::env::var("SNIP_E2E_RPC_URL").unwrap_or_else(|_| E2E_RPC_URL_DEFAULT.to_string())
}

/// Path to the operator-supplied `e2e_keys/` directory at the SNIP
/// repo root. Override per-test via `SNIP_E2E_KEYS_DIR`.
pub fn e2e_keys_dir() -> std::path::PathBuf {
    std::env::var("SNIP_E2E_KEYS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            // CARGO_MANIFEST_DIR is the path of the crate being
            // tested (= crates/sum-node). The keys dir lives at
            // the SNIP repo root, two levels up.
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("e2e_keys")
        })
}

/// Read a role's seed file produced by `e2e-helper generate-e2e-keys`
/// and decode the hex into 32 bytes.
pub fn read_e2e_seed(role: &str) -> [u8; 32] {
    let path = e2e_keys_dir().join(format!("{role}.seed.hex"));
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "e2e: missing seed file at {}: {e}\n\
             Generate with: cargo run -p sum-node --bin e2e-helper -- \\\n\
             \tgenerate-e2e-keys --out e2e_keys\n\
             Then bring up the mirror with the overlay (see\n\
             docs/OPERATOR-RUNBOOK.md `Funded test accounts`).",
            path.display()
        )
    });
    let trimmed = content.trim();
    let bytes = hex::decode(trimmed)
        .unwrap_or_else(|e| panic!("e2e: seed file {} not valid hex: {e}", path.display()));
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    seed
}

/// Derive the L1 base58 address for a role's seed.
pub fn role_address(role: &str) -> String {
    let seed = read_e2e_seed(role);
    let kp = sum_net::identity::keypair_from_seed(&seed)
        .unwrap_or_else(|e| panic!("e2e: keypair_from_seed failed for role {role}: {e}"));
    let addr = sum_net::identity::l1_address_from_keypair(&kp);
    sum_net::identity::l1_address_base58(&addr)
}

/// Verify the mirror is reachable + V2 enabled + finality is
/// advancing. Panics with an actionable message if any check fails.
/// Each WS2b scenario should call this as its first action.
pub async fn mirror_running_or_skip(rpc: &L1RpcClient) {
    let params = rpc.chain_get_chain_params().await.unwrap_or_else(|e| {
        panic!(
            "e2e: chain_getChainParams failed against {}: {e}\n\
             Is the local mirror up? Try: make smoke RPC=<url>",
            e2e_rpc_url()
        )
    });
    assert_eq!(
        params.chain_id, 1337,
        "e2e: mirror reports chain_id={}, expected 1337 (per docs/CHAIN-COMPAT.md)",
        params.chain_id
    );
    let v2 = params.v2_enabled_from_height;
    assert!(
        matches!(v2, Some(0)),
        "e2e: mirror reports v2_enabled_from_height={v2:?}, expected Some(0). \
         Mirror genesis didn't enable V2 — chain-compat issue."
    );

    let head = rpc
        .chain_get_block_height()
        .await
        .unwrap_or_else(|e| panic!("e2e: chain_getBlockHeight failed: {e}"));
    assert_eq!(
        head.finality, "finalized",
        "e2e: mirror returned finality={}, expected \"finalized\"",
        head.finality
    );
    // height==0 is allowed (fresh-genesis mirror at first poll); the
    // tx-finality scenarios will time out their finality budgets if
    // the chain isn't actually advancing.
}

/// Assert the role's address has a non-zero balance on the mirror.
/// If zero, panic with the exact bring-up command operators need.
pub async fn funded_or_skip(rpc: &L1RpcClient, role: &str) {
    let addr = role_address(role);
    let bal_str = rpc
        .call_public::<serde_json::Value>("get_balance", serde_json::json!([&addr]))
        .await
        .unwrap_or_else(|e| panic!("e2e: get_balance({addr}) failed: {e}"))
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let bal: u128 = bal_str.parse().unwrap_or(0);
    assert!(
        bal > 0,
        "e2e: role={role} address={addr} has balance=0.\n\
         The mirror was not brought up with this overlay, OR the chain DB\n\
         existed when the overlay was mounted (overlay is fresh-genesis only).\n\
         To fix:\n\
           cd <chain-repo>\n\
           docker-compose -f deploy/snip-local-mirror.yaml down -v\n\
           # ensure deploy/extra-alloc.json contains your e2e_keys addresses\n\
           docker-compose \\\n\
               -f deploy/snip-local-mirror.yaml \\\n\
               -f deploy/snip-local-mirror.override.yaml \\\n\
               up -d --build\n\
         See docs/OPERATOR-RUNBOOK.md \"Funded test accounts\" for the full flow."
    );
}

/// Poll `chain_getTransactionStatus` until the tx is `Finalized`,
/// or fail with a budget-exceeded error after
/// `E2E_FINALITY_BUDGET_SECS`. Real wall-clock — WS2b's manual
/// gate is the only place sleeps are acceptable in this repo.
pub async fn await_tx_finality(rpc: &L1RpcClient, tx_hash: &str) -> u64 {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(E2E_FINALITY_BUDGET_SECS);
    let mut last_status = "unknown".to_string();
    while tokio::time::Instant::now() < deadline {
        match rpc.chain_get_transaction_status(tx_hash).await {
            Ok(TxStatusV2::Finalized { block_height }) => return block_height,
            Ok(TxStatusV2::Failed {
                block_height,
                reason,
            }) => panic!("e2e: tx {tx_hash} failed at {block_height:?}: {reason}"),
            Ok(TxStatusV2::Dropped) => panic!("e2e: tx {tx_hash} dropped from mempool"),
            Ok(other) => {
                last_status = format!("{other:?}");
            }
            Err(e) => {
                last_status = format!("Err({e})");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    panic!(
        "e2e: tx {tx_hash} did not finalize within {}s budget (last status: {last_status})",
        E2E_FINALITY_BUDGET_SECS
    );
}

/// Spawn `sum-node` (or `e2e-helper`) as a subprocess, capturing
/// output. Path comes from cargo's `CARGO_BIN_EXE_<name>` env var,
/// guaranteeing the test runs against the same build artifact
/// `cargo test` produced. Returns `Output` so the caller can assert
/// on `status` + parse `stdout` / `stderr`.
///
/// Each spawn carries a clean env to insulate from operator shell
/// state — only `RUST_LOG` and the explicitly-supplied env vars
/// pass through.
pub fn spawn_bin(
    bin_name: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let bin_path = match bin_name {
        "sum-node" => env!("CARGO_BIN_EXE_sum-node"),
        "e2e-helper" => env!("CARGO_BIN_EXE_e2e-helper"),
        other => panic!("e2e: spawn_bin unknown binary {other}"),
    };
    let mut cmd = std::process::Command::new(bin_path);
    cmd.env_clear();
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        cmd.env("RUST_LOG", rust_log);
    }
    // `PATH` minimally needed so subprocess can locate dynamic
    // loader / libc / etc on the host.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(args);
    cmd.output()
        .unwrap_or_else(|e| panic!("e2e: spawn_bin({bin_name}) failed: {e}"))
}

/// Convenience: write a hex seed to a tempfile and return its path.
/// Used by tests that need to pass `--key-file <path>` to a
/// subprocess. The tempfile drops at end-of-scope, removing the
/// seed from disk.
pub fn write_seed_tempfile(seed: &[u8; 32]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("e2e: tempfile");
    use std::io::Write;
    writeln!(f.as_file_mut(), "{}", hex::encode(seed)).expect("e2e: write seed");
    f
}

/// Path to the seed file for `role` inside `e2e_keys/` (read-only).
/// Returned as a `PathBuf` for convenience; tests pass it directly
/// as `--key-file`.
pub fn role_seed_path(role: &str) -> std::path::PathBuf {
    e2e_keys_dir().join(format!("{role}.seed.hex"))
}

/// Cargo workspace root path (parent of `crates/`). Tests use this
/// when they need to invoke `sum-node` from a known CWD.
pub fn workspace_root() -> std::path::PathBuf {
    StdPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("e2e: workspace root canonicalize")
}

// ── Sanity self-tests ───────────────────────────────────────────────────────
//
// The helper module itself doesn't run as a test crate; these guard
// the helper invariants the lifecycle tests rely on. Per clippy
// `items_after_test_module`, this block stays at the bottom of the
// file.

#[cfg(test)]
mod helpers_self_test {
    use super::*;

    #[test]
    fn test_seeds_yield_distinct_x25519_pubkeys() {
        let owner_pk = x25519_pub_for_seed(&OWNER_SEED);
        let recip_pk = x25519_pub_for_seed(&RECIPIENT_SEED);
        let third_pk = x25519_pub_for_seed(&THIRD_PARTY_SEED);
        assert_ne!(owner_pk, recip_pk);
        assert_ne!(owner_pk, third_pk);
        assert_ne!(recip_pk, third_pk);
    }

    #[test]
    fn build_private_artifacts_round_trip_root_is_stable() {
        // Same plaintext + same K_file → same merkle_root every call.
        // (Determinism is the load-bearing property for resume
        // recovery scenarios.)
        let k = make_test_kfile();
        let pt = small_plaintext();
        let a = build_private_test_artifacts(&pt, &k);
        let b = build_private_test_artifacts(&pt, &k);
        assert_eq!(a.merkle_root, b.merkle_root);
    }

    #[test]
    fn bundle_size_matches_recipient_bundle_size() {
        let entry = make_access_entry(
            &RECIPIENT_ADDR,
            &x25519_pub_for_seed(&RECIPIENT_SEED),
            &make_test_kfile(),
            None,
        );
        let bundle = entry.encrypted_key_bundle.expect("bundle present");
        let stripped = bundle.strip_prefix("0x").unwrap();
        assert_eq!(stripped.len(), RECIPIENT_BUNDLE_SIZE * 2);
    }
}
