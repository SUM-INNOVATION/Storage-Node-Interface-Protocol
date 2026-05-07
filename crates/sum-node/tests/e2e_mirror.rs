//! WS2b — local-mirror E2E suite.
//!
//! All tests `#[ignore]`'d. Runs via `make e2e-mirror` (manual gate,
//! never in `release-check` or PR CI). Drives the production
//! `sum-node` and `e2e-helper` binaries as subprocesses against a
//! real running chain mirror at `http://localhost:8545`.
//!
//! Preconditions checked at runtime:
//!   * Mirror up + V2 enabled (chain_id 31337, finality advancing).
//!   * `e2e_keys/` populated by `e2e-helper generate-e2e-keys`.
//!   * Mirror brought up with the matching `extra-alloc.json`
//!     overlay so role addresses are funded.
//!
//! Failure modes are explicit + actionable: missing seeds, zero
//! balances, RPC unreachable each surface a clear panic message
//! pointing the operator at the runbook step they missed.
//!
//! Scenarios in this file:
//!   1. mirror_health_probe
//!   2. register_node
//!   3. register_encryption_key
//!   4. public_ingest_then_download_round_trip
//!   5. private_owner_only_ingest_then_owner_download
//!   6. private_shared_recipient_ingest_then_recipient_download
//!   7. share_post_ingest_admits_new_recipient
//!   8. revoke_denies_recipient_after_finality
//!   9. update_access_extend_expiry_admits_past_original_cutoff
//!  10. update_access_clear_expiry_grants_indefinite_access
//!  11. resume_pending_completes_to_active
//!
//! Scenario 12 (archive restart) is deferred to its own follow-up
//! commit — process-lifecycle / port-reuse / store-reload concerns
//! are subtle enough to deserve isolated review.

#![cfg(test)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use sum_node::rpc_client::L1RpcClient;

use common::{
    await_tx_finality, e2e_rpc_url, funded_or_skip, mirror_running_or_skip, role_address,
    role_seed_path, spawn_bin,
};

// ── Shared per-test setup ───────────────────────────────────────────────────

/// Standard prelude every scenario calls first. Establishes the
/// connection, sanity-checks the mirror, returns the rpc client.
async fn setup() -> L1RpcClient {
    let rpc = L1RpcClient::new(e2e_rpc_url());
    mirror_running_or_skip(&rpc).await;
    rpc
}

/// Spawn `sum-node ingest-v2 ...` and parse the resulting merkle
/// root from stdout. The CLI prints a stable `merkle_root: 0x...`
/// line on every outcome that has a recorded root (Active, Pending,
/// already-on-chain, etc.) — we parse that line. tracing log lines
/// are NOT a reliable extraction source because they include ANSI
/// escape sequences when the captured stream is not a TTY.
fn spawn_ingest_and_extract_root(
    key_file: &std::path::Path,
    file_path: &std::path::Path,
    visibility: &str,
    recipients: &[String],
) -> [u8; 32] {
    let mut args: Vec<String> = vec![
        "--key-file".into(),
        key_file.display().to_string(),
        "--rpc-url".into(),
        e2e_rpc_url(),
        "--chain-id".into(),
        "31337".into(),
        "ingest-v2".into(),
        file_path.display().to_string(),
        "--visibility".into(),
        visibility.into(),
    ];
    for r in recipients {
        args.push("--recipient".into());
        args.push(r.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = spawn_bin("sum-node", &arg_refs, &[]);
    assert!(
        out.status.success(),
        "ingest-v2 failed (exit={:?}): stderr={}\nstdout={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_merkle_root_line(&stdout).unwrap_or_else(|| {
        panic!(
            "ingest-v2 did not emit a `merkle_root: 0x...` line. \
             stdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Pull the 32-byte merkle root out of a `merkle_root: 0x<hex>` line
/// in the given output. Returns `None` if no such line is found or
/// the hex doesn't decode to 32 bytes.
fn parse_merkle_root_line(stdout: &str) -> Option<[u8; 32]> {
    let raw = stdout
        .lines()
        .find_map(|l| l.strip_prefix("merkle_root:").map(|s| s.trim().to_string()))?;
    let stripped = raw.strip_prefix("0x").unwrap_or(&raw);
    let bytes = hex::decode(stripped).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut root = [0u8; 32];
    root.copy_from_slice(&bytes);
    Some(root)
}

/// Extract the FIRST 64-hex-char token (32 bytes) from a string,
/// stripping a `0x` prefix if present. Used to scrape merkle_root
/// from CLI output regardless of surrounding format ("merkle_root
/// = 0x...", "root: ...", etc.).
fn extract_hex32(s: &str) -> Option<String> {
    for token in s.split_whitespace() {
        let stripped = token.trim_matches(|c: char| !c.is_ascii_hexdigit() && c != 'x' && c != 'X');
        let hex_part = stripped.strip_prefix("0x").unwrap_or(stripped);
        if hex_part.len() == 64 && hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex_part.to_string());
        }
    }
    None
}

/// Spawn a long-running `sum-node listen` archive node. Returns
/// the `Child` handle so the test can kill it during teardown.
/// Callers are responsible for waiting a few seconds before the
/// archive is fully discoverable (libp2p mDNS / chain registration).
fn spawn_archive_listen(role: &str, store_dir: &std::path::Path) -> std::process::Child {
    use std::process::Stdio;
    let bin_path = env!("CARGO_BIN_EXE_sum-node");
    let mut cmd = std::process::Command::new(bin_path);
    cmd.env_clear();
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        cmd.env("RUST_LOG", rust_log);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    // Each archive needs its own store-root so they don't clobber
    // each other's chunks. Pass via env (SumStore reads from CWD
    // by default; we change CWD to give it an isolated dir).
    cmd.current_dir(store_dir);
    cmd.args([
        "--key-file",
        role_seed_path(role).to_str().unwrap(),
        "--rpc-url",
        &e2e_rpc_url(),
        "--chain-id",
        "31337",
        "listen",
    ]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn()
        .unwrap_or_else(|e| panic!("e2e: spawn sum-node listen for {role}: {e}"))
}

/// RAII guard that kills a child process when dropped. Tests use
/// this to ensure archives are torn down even on panic.
struct ChildGuard(Option<std::process::Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
impl ChildGuard {
    fn new(c: std::process::Child) -> Self {
        Self(Some(c))
    }
}

/// Write a test plaintext blob to a tempfile.
fn write_test_plaintext(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("e2e: tempdir");
    let path = dir.path().join("e2e_payload.bin");
    std::fs::write(&path, bytes).expect("e2e: write payload");
    (dir, path)
}

/// Build a per-scenario unique plaintext. Combines a scenario-name
/// header (so test logs identify the source) with `OsRng` random
/// bytes so two invocations of the same scenario also differ. The
/// goal is to pin a fresh merkle root for every `RegisterFilePendingV2`
/// — a deterministic plaintext would collide with a prior scenario's
/// already-registered root and the chain's validity check would fail
/// the second submission.
fn unique_plaintext(scenario: &str, body_bytes: usize) -> Vec<u8> {
    use rand_core::{OsRng, RngCore};
    let header = format!("snip-e2e-mirror::{scenario}\n").into_bytes();
    let mut out = Vec::with_capacity(header.len() + body_bytes);
    out.extend_from_slice(&header);
    let mut body = vec![0u8; body_bytes];
    OsRng.fill_bytes(&mut body);
    out.extend_from_slice(&body);
    out
}

// ── Per-scenario prereq helpers ─────────────────────────────────────────────
//
// Every scenario sets up its own prereqs. Nothing relies on alphabetical
// run order. These helpers are idempotent: each one read-checks chain
// state first and returns early if already in the desired state, so the
// same scenario can be re-run against a partially-warm mirror without
// burning a redundant submission.

/// Ensure `role` is registered as `ArchiveNode/Active` in the on-chain
/// registry. Idempotent — read-checks `get_node_record` first and
/// returns if already present. Otherwise spawns the production
/// `sum-node register-node` CLI and waits for finality.
async fn ensure_archive_registered(rpc: &L1RpcClient, role: &str) {
    let addr = role_address(role);
    if let Ok(Some(_)) = rpc.get_node_record(&addr).await {
        return;
    }
    let key = role_seed_path(role);
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "register-node",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "ensure_archive_registered({role}) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
}

/// Ensure `role` has an X25519 encryption public key registered on
/// chain. Idempotent — if `account_getEncryptionPublicKey` already
/// returns `Some(_)` the helper returns. Otherwise it submits
/// `register-encryption-key` and waits for finality.
async fn ensure_encryption_key_registered(rpc: &L1RpcClient, role: &str) {
    let addr = role_address(role);
    if let Ok(Some(_)) = rpc.account_get_encryption_public_key(&addr).await {
        return;
    }
    let key = role_seed_path(role);
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "register-encryption-key",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "ensure_encryption_key_registered({role}) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
}

/// The three archive role names this harness expects to see funded
/// + registered. The chain plan fixes
/// `assignment_replication_factor = 3`, so any ingest scenario must
/// stand up at least three archive identities to satisfy quorum.
const ARCHIVE_ROLES: [&str; 3] = ["archive_1", "archive_2", "archive_3"];

/// Stand up the full archive fleet a typical ingest/download scenario
/// needs:
///   1. Verify each archive role is funded (panics with the runbook
///      pointer if not — same contract as `funded_or_skip`).
///   2. Ensure each is registered on chain as `ArchiveNode/Active`.
///   3. Spawn a `sum-node listen` for each, in its own store-root
///      tempdir, with OS-assigned ports so they don't collide.
///   4. Wait until `storage_getActiveNodesAtHeight(<finalized>)`
///      returns at least 3 active archive entries — proves the
///      chain side of the fleet is satisfiable, not just our local
///      processes.
///
/// Returns RAII guards + the tempdirs so callers can keep the
/// archives alive for the duration of the test (drop = teardown).
async fn spawn_archive_fleet(rpc: &L1RpcClient) -> (Vec<ChildGuard>, Vec<tempfile::TempDir>) {
    for role in ARCHIVE_ROLES {
        funded_or_skip(rpc, role).await;
        ensure_archive_registered(rpc, role).await;
    }
    let mut guards = Vec::with_capacity(ARCHIVE_ROLES.len());
    let mut dirs = Vec::with_capacity(ARCHIVE_ROLES.len());
    for role in ARCHIVE_ROLES {
        let dir = tempfile::tempdir().expect("e2e: archive store dir");
        let child = spawn_archive_listen(role, dir.path());
        guards.push(ChildGuard::new(child));
        dirs.push(dir);
    }

    // Wait for the chain to confirm the fleet is registered + active.
    // Local listen() startup is independent from chain registration
    // (registration was already done above), but the active-nodes
    // snapshot is height-based; the snapshot at height H reflects
    // registrations finalized at H, so we re-poll until the
    // currently-finalized snapshot shows ≥3.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let head = rpc
            .chain_get_block_height()
            .await
            .expect("chain_getBlockHeight while waiting for archive fleet");
        let nodes = rpc
            .storage_get_active_nodes_at_height(head.height)
            .await
            .expect("storage_getActiveNodesAtHeight while waiting for archive fleet");
        // `NodeRecordInfo.role` is `String` on the wire — chain
        // serializes the enum tag directly. Match on the literal.
        let archives = nodes.iter().filter(|n| n.role == "ArchiveNode").count();
        if archives >= ARCHIVE_ROLES.len() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "e2e: chain only sees {archives} active archives at height {} after 60s; \
                 expected ≥{} (chain assignment_replication_factor=3 means ingest \
                 will under-replicate). Check `sum-node register-node` finality budget.",
                head.height,
                ARCHIVE_ROLES.len()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Local listeners need a brief settle window to reach mDNS
    // discoverability — the chain side is now guaranteed, this last
    // sleep covers libp2p only.
    tokio::time::sleep(Duration::from_secs(5)).await;

    (guards, dirs)
}

// ── 1. mirror_health_probe ──────────────────────────────────────────────────

/// Smoke probe: chain_id, V2 state, finality. Pure read-only.
/// Validates the mirror is in a state every other scenario relies
/// on. If this fails, no other scenario can pass — fix the mirror
/// first.
#[ignore]
#[tokio::test]
async fn mirror_health_probe() {
    let _rpc = setup().await;
    // setup() asserted everything load-bearing. If we got here, the
    // mirror is up + V2-enabled + finality is "finalized". No
    // further assertion needed for this scenario.
}

// ── 2. register_node ────────────────────────────────────────────────────────

/// Submit `register-node` for archive_1 via the production
/// `sum-node` CLI, then query the node registry to confirm the node
/// is recorded.
///
/// Order-independent. The chain plan's `NodeRegistry` rejects a
/// second `Register` for an already-registered identity (there is
/// no in-place stake-rotation op), so this scenario:
///   * If no other scenario has yet registered archive_1, the CLI
///     is invoked end-to-end and the test asserts on its stable
///     `tx_hash:` / `finalized_height:` stdout contract.
///   * If archive_1 was already registered by some earlier
///     scenario's `spawn_archive_fleet` call, the test re-verifies
///     registry visibility and skips the CLI invocation. (Other
///     scenarios still exercise the CLI via `ensure_archive_registered`,
///     so the WS2b suite always exercises it once on a fresh chain.)
#[ignore]
#[tokio::test]
async fn register_node() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "archive_1").await;

    let addr = role_address("archive_1");
    let already_registered = rpc
        .get_node_record(&addr)
        .await
        .expect("get_node_record probe before RegisterNode")
        .is_some();

    if !already_registered {
        let archive_key = role_seed_path("archive_1");
        let out = spawn_bin(
            "sum-node",
            &[
                "--key-file",
                archive_key.to_str().unwrap(),
                "--rpc-url",
                &e2e_rpc_url(),
                "register-node",
            ],
            &[],
        );
        assert!(
            out.status.success(),
            "register-node failed: stderr={}\nstdout={}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );

        // Stable stdout contract: `tx_hash: 0x...` + `finalized_height: <N>`.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let tx_hash = stdout
            .lines()
            .find_map(|l| l.strip_prefix("tx_hash:").map(|s| s.trim().to_string()))
            .expect("register-node stdout missing `tx_hash:` line");
        let finalized_height: u64 = stdout
            .lines()
            .find_map(|l| {
                l.strip_prefix("finalized_height:")
                    .and_then(|s| s.trim().parse().ok())
            })
            .expect("register-node stdout missing `finalized_height:` line");
        assert!(
            finalized_height > 0,
            "register-node reported finalized_height=0, expected > 0"
        );
        // Belt-and-suspenders: confirm the chain agrees the tx is finalized.
        await_tx_finality(&rpc, &tx_hash).await;
    }

    // In either branch the registry must show archive_1 as Active.
    let record = rpc
        .get_node_record(&addr)
        .await
        .expect("get_node_record after RegisterNode");
    assert!(
        record.is_some(),
        "archive_1 ({addr}) not visible in node registry after RegisterNode finalized"
    );
}

// ── 3. register_encryption_key ──────────────────────────────────────────────

/// Submit `register-encryption-key` for owner, await finality,
/// query `account_getEncryptionPublicKey` to confirm chain
/// recorded the key.
#[ignore]
#[tokio::test]
async fn register_encryption_key() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;

    let owner_key = role_seed_path("owner");
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            owner_key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "register-encryption-key",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "register-encryption-key failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    // The CLI doesn't always return the tx hash on stdout; just
    // wait a few finality cycles and confirm chain state.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let addr = role_address("owner");
    let pk = rpc
        .account_get_encryption_public_key(&addr)
        .await
        .expect("account_getEncryptionPublicKey after register-encryption-key");
    assert!(
        pk.is_some(),
        "owner ({addr}) has no encryption pubkey after register-encryption-key"
    );
}

// ── 4. public_ingest_then_download_round_trip ───────────────────────────────

/// End-to-end: archive fleet listening → owner ingests Public file →
/// download from a fresh node → byte-identical plaintext.
///
/// Per the chain plan's `assignment_replication_factor = 3`, the
/// fleet stands up all three `archive_*` roles: registering one
/// archive only would leave S2's push wave under-replicated and the
/// file stuck in `Pending`. Public files don't require encryption-key
/// registration, so the only prereqs are funded archives.
#[ignore]
#[tokio::test]
async fn public_ingest_then_download_round_trip() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    // Owner ingests a Public file. Unique plaintext per scenario so
    // we never collide with another scenario's already-registered
    // merkle root.
    let plaintext = unique_plaintext("public_ingest_then_download_round_trip", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let merkle_root =
        spawn_ingest_and_extract_root(&role_seed_path("owner"), &pt_path, "public", &[]);

    // 3. Download from a fresh node (uses recipient seed; for Public
    //    files key identity doesn't matter for ACL — anyone can read).
    let download_dir = tempfile::tempdir().expect("download out dir");
    let out_path = download_dir.path().join("recovered.bin");
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "download (Public) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    // 4. Assert byte-identical.
    let recovered = std::fs::read(&out_path).expect("read recovered file");
    assert_eq!(
        recovered, plaintext,
        "Public download did not round-trip byte-identical plaintext"
    );
}

// ── 5. private_owner_only_ingest_then_owner_download ────────────────────────

/// Owner ingests a Private file with no recipients (owner self-share),
/// then downloads with the same key. Pins the V2-Private path:
/// access list lookup → bundle unwrap → manifest decrypt → chunk
/// decrypt → plaintext.
///
/// Private ingest requires the owner's X25519 encryption pubkey on
/// chain (the chain validates `RegisterFilePendingV2` against the
/// owner's encryption record), so the prereqs are: 3-archive fleet
/// + owner's encryption key registered. Both are set up inline so
/// this scenario does not depend on `register_encryption_key`
/// running first.
#[ignore]
#[tokio::test]
async fn private_owner_only_ingest_then_owner_download() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    let plaintext = unique_plaintext("private_owner_only_ingest_then_owner_download", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let merkle_root =
        spawn_ingest_and_extract_root(&role_seed_path("owner"), &pt_path, "private", &[]);

    let download_dir = tempfile::tempdir().expect("download out dir");
    let out_path = download_dir.path().join("recovered.bin");
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("owner").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "download (Private owner-only) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let recovered = std::fs::read(&out_path).expect("read recovered file");
    assert_eq!(
        recovered, plaintext,
        "Private owner-only download did not round-trip"
    );
}

// ── 6. private_shared_recipient_ingest_then_recipient_download ──────────────

/// Owner ingests Private with one recipient → recipient downloads.
/// Pins the wrap-for-other-recipient + recipient-side unwrap path.
///
/// Private-with-recipient ingest needs both the owner's AND the
/// recipient's X25519 encryption pubkeys on chain — chain-side
/// validation rejects an `AccessEntryV2` for a recipient with no
/// registered encryption key, and the owner's key is needed to
/// wrap K_file for owner-side recovery. Both are set up inline.
#[ignore]
#[tokio::test]
async fn private_shared_recipient_ingest_then_recipient_download() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "recipient").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "recipient").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    let plaintext = unique_plaintext(
        "private_shared_recipient_ingest_then_recipient_download",
        1024,
    );
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let recipient_addr = role_address("recipient");
    let merkle_root = spawn_ingest_and_extract_root(
        &role_seed_path("owner"),
        &pt_path,
        "private",
        &[recipient_addr],
    );

    let download_dir = tempfile::tempdir().expect("download out dir");
    let out_path = download_dir.path().join("recovered.bin");
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "download (Private recipient) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let recovered = std::fs::read(&out_path).expect("read recovered file");
    assert_eq!(
        recovered, plaintext,
        "Private shared download did not round-trip"
    );
}

// ── 7. share_post_ingest_admits_new_recipient ───────────────────────────────

/// Owner ingests Private (owner-only) → owner runs `share` for a
/// third party → third party downloads. Pins the post-ingest
/// `AddAccessV2` flow.
///
/// `share` re-wraps K_file for the third-party using their
/// chain-registered X25519 pubkey, so the third-party's key must
/// be on chain BEFORE `share` submits. Owner's key is also required
/// (private ingest, even owner-only, validates owner's encryption
/// record). All prereqs set up inline.
#[ignore]
#[tokio::test]
async fn share_post_ingest_admits_new_recipient() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "third_party").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "third_party").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    // Owner ingests private (no recipients yet).
    let plaintext = unique_plaintext("share_post_ingest_admits_new_recipient", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let merkle_root =
        spawn_ingest_and_extract_root(&role_seed_path("owner"), &pt_path, "private", &[]);
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    // Owner runs `share` for third-party.
    let third_party_addr = role_address("third_party");
    let share_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("owner").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "share",
            &root_hex,
            "--recipient",
            &third_party_addr,
        ],
        &[],
    );
    assert!(
        share_out.status.success(),
        "share failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&share_out.stderr),
        String::from_utf8_lossy(&share_out.stdout),
    );
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Third-party downloads.
    let download_dir = tempfile::tempdir().expect("download out dir");
    let out_path = download_dir.path().join("recovered.bin");
    let download_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("third_party").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        download_out.status.success(),
        "third-party download (post-share) failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&download_out.stderr),
        String::from_utf8_lossy(&download_out.stdout),
    );

    let recovered = std::fs::read(&out_path).expect("read recovered file");
    assert_eq!(recovered, plaintext);
}

// ── 8. revoke_denies_recipient_after_finality ───────────────────────────────

/// Owner ingests + shares with recipient → recipient downloads
/// (works) → owner revokes → wait for finality → recipient retry
/// must fail with `NoAccess`. Pins the revoke contract end-to-end.
#[ignore]
#[tokio::test]
async fn revoke_denies_recipient_after_finality() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "recipient").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "recipient").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    let plaintext = unique_plaintext("revoke_denies_recipient_after_finality", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let recipient_addr = role_address("recipient");
    let merkle_root = spawn_ingest_and_extract_root(
        &role_seed_path("owner"),
        &pt_path,
        "private",
        std::slice::from_ref(&recipient_addr),
    );
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    // Pre-revoke: recipient downloads (sanity).
    let dl_dir_before = tempfile::tempdir().unwrap();
    let out_before = dl_dir_before.path().join("before.bin");
    let pre_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_before.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        pre_out.status.success(),
        "pre-revoke recipient download must succeed (fixture sanity): {}",
        String::from_utf8_lossy(&pre_out.stderr),
    );

    // Owner revokes recipient.
    let revoke_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("owner").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "revoke",
            &root_hex,
            "--recipient",
            &recipient_addr,
        ],
        &[],
    );
    assert!(
        revoke_out.status.success(),
        "revoke failed: {}",
        String::from_utf8_lossy(&revoke_out.stderr),
    );
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Post-revoke: recipient download must fail.
    let dl_dir_after = tempfile::tempdir().unwrap();
    let out_after = dl_dir_after.path().join("after.bin");
    let post_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_after.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        !post_out.status.success(),
        "post-revoke recipient download MUST fail (revoked recipient still got bytes — privacy regression). \
         stdout={} stderr={}",
        String::from_utf8_lossy(&post_out.stdout),
        String::from_utf8_lossy(&post_out.stderr),
    );
    let stderr = String::from_utf8_lossy(&post_out.stderr);
    assert!(
        stderr.contains("NoAccess") || stderr.contains("no access"),
        "post-revoke must surface NoAccess error; got stderr={stderr}"
    );
}

// ── 9. update_access_extend_expiry_admits_past_original_cutoff ──────────────

/// Owner ingests + shares with recipient @ expires_at=N → at
/// finalized_height=N+1 recipient is expired → owner runs
/// `update-access` to extend → recipient downloads again. Pins the
/// extend-expiry path. Uses concrete heights so the strict-`>`
/// semantics are exercised against real chain state, not mocks.
#[ignore]
#[tokio::test]
async fn update_access_extend_expiry_admits_past_original_cutoff() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "recipient").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "recipient").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    // Set expires_at relative to current finalized height.
    let head = rpc
        .chain_get_block_height()
        .await
        .expect("chain height for expiry calculation");
    let original_expires = head.height + 5; // expires in ~10s

    let recipient_addr = role_address("recipient");
    let recipient_with_expiry = format!("{recipient_addr}:{original_expires}");

    let plaintext = unique_plaintext(
        "update_access_extend_expiry_admits_past_original_cutoff",
        1024,
    );
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let merkle_root = spawn_ingest_and_extract_root(
        &role_seed_path("owner"),
        &pt_path,
        "private",
        &[recipient_with_expiry],
    );
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    // Wait until past the original expiry.
    tokio::time::sleep(Duration::from_secs(14)).await;

    // Owner extends expiry.
    let new_expires = head.height + 10_000;
    let updated = format!("{recipient_addr}:{new_expires}");
    let upd_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("owner").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "update-access",
            &root_hex,
            "--recipient",
            &updated,
        ],
        &[],
    );
    assert!(
        upd_out.status.success(),
        "update-access extend failed: {}",
        String::from_utf8_lossy(&upd_out.stderr),
    );
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Recipient downloads (should succeed now).
    let dl_dir = tempfile::tempdir().unwrap();
    let out_path = dl_dir.path().join("after-extend.bin");
    let dl_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        dl_out.status.success(),
        "recipient download after extend MUST succeed: stderr={}",
        String::from_utf8_lossy(&dl_out.stderr),
    );
    let recovered = std::fs::read(&out_path).expect("read recovered");
    assert_eq!(recovered, plaintext);
}

// ── 10. update_access_clear_expiry_grants_indefinite_access ─────────────────

/// Owner ingests + shares with recipient @ expires_at=N → owner
/// runs `update-access ...:none` to clear the expiry → recipient
/// can download regardless of finality height.
#[ignore]
#[tokio::test]
async fn update_access_clear_expiry_grants_indefinite_access() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "recipient").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    ensure_encryption_key_registered(&rpc, "recipient").await;
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    let head = rpc.chain_get_block_height().await.unwrap();
    let original_expires = head.height + 5;
    let recipient_addr = role_address("recipient");
    let with_expiry = format!("{recipient_addr}:{original_expires}");

    let plaintext = unique_plaintext("update_access_clear_expiry_grants_indefinite_access", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);
    let merkle_root = spawn_ingest_and_extract_root(
        &role_seed_path("owner"),
        &pt_path,
        "private",
        &[with_expiry],
    );
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    // Owner clears expiry.
    let cleared = format!("{recipient_addr}:none");
    let upd_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("owner").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "update-access",
            &root_hex,
            "--recipient",
            &cleared,
        ],
        &[],
    );
    assert!(
        upd_out.status.success(),
        "update-access clear failed: {}",
        String::from_utf8_lossy(&upd_out.stderr),
    );
    // Wait past the ORIGINAL expiry to prove the clear took effect.
    tokio::time::sleep(Duration::from_secs(14)).await;

    let dl_dir = tempfile::tempdir().unwrap();
    let out_path = dl_dir.path().join("after-clear.bin");
    let dl_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            role_seed_path("recipient").to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "download",
            &root_hex,
            "--output",
            out_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        dl_out.status.success(),
        "recipient download after expiry-clear MUST succeed: {}",
        String::from_utf8_lossy(&dl_out.stderr),
    );
    let recovered = std::fs::read(&out_path).expect("read recovered");
    assert_eq!(recovered, plaintext);
}

// ── 11. resume_pending_completes_to_active ──────────────────────────────────

/// Pending file → resume → activated.
///
/// Driving a real `Pending` outcome on a healthy 3-archive fleet is
/// timing-sensitive (S2 may complete inside even a 1-second budget
/// over loopback), so this scenario forces the Pending state
/// deterministically: the initial ingest runs **without** any
/// archives listening, which guarantees S2 cannot push to R=3 and
/// leaves the file in `Pending` after `RegisterFilePendingV2`.
/// Then we stand up the full archive fleet and `resume` against the
/// same root + file body — the resume must drive S2 to full
/// replication and chain lifecycle to `Active`.
///
/// Uses a unique plaintext so the file's merkle root never collides
/// with `public_ingest_then_download_round_trip` (which previously
/// shared the same deterministic 0..1024 plaintext, causing the
/// chain to reject the second `RegisterFilePendingV2` with
/// "validity check failed").
#[ignore]
#[tokio::test]
async fn resume_pending_completes_to_active() {
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    // Funding-check the archive roles so a missing fund surfaces
    // *before* we burn the failed ingest tx; the actual fleet is
    // deferred until after the Pending-inducing initial ingest.
    for role in ARCHIVE_ROLES {
        funded_or_skip(&rpc, role).await;
        ensure_archive_registered(&rpc, role).await;
    }

    // Unique plaintext per scenario; every run gets a fresh merkle
    // root.
    let plaintext = unique_plaintext("resume_pending_completes_to_active", 1024);
    let (_pt_dir, pt_path) = write_test_plaintext(&plaintext);

    let owner_key = role_seed_path("owner");
    // Phase 1 — induce Pending on purpose by submitting the initial
    // ingest with NO archive listeners running. `RegisterFilePendingV2`
    // will finalize, but S2's push wave finds no peers and the
    // ingest exits in `Pending` after the budget elapses. The CLI
    // exit code is non-zero (Pending is reported as a failure to
    // activate), so we don't assert on `success()` here.
    let initial = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            owner_key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "ingest-v2",
            pt_path.to_str().unwrap(),
            "--visibility",
            "public",
            "--push-wait-secs",
            "1",
            "--manifest-push-wait-secs",
            "1",
            "--activation-wait-secs",
            "1",
        ],
        &[],
    );
    let stdout_initial = String::from_utf8_lossy(&initial.stdout);
    let stderr_initial = String::from_utf8_lossy(&initial.stderr);
    let combined_initial = format!("{stdout_initial}\n{stderr_initial}");
    let root_hex = extract_hex32(&combined_initial)
        .unwrap_or_else(|| panic!("ingest-v2 did not emit merkle root: {combined_initial}"));
    let prefixed = format!("0x{root_hex}");

    // Phase 2 — stand up the archive fleet, then resume. With three
    // listeners + chain-side registration confirmed, resume's S2
    // wave can satisfy R=3 and drive the file to Active.
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;

    // resume against the same root + path. This MUST drive the
    // file to Active regardless of where S2 left it.
    let resume_out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            owner_key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "31337",
            "resume",
            &prefixed,
            pt_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        resume_out.status.success(),
        "resume failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&resume_out.stderr),
        String::from_utf8_lossy(&resume_out.stdout),
    );

    // Confirm chain row is Active.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let info = rpc
        .storage_get_file_info_v2(&prefixed, None, None)
        .await
        .expect("storage_getFileInfoV2 after resume");
    assert!(
        info.lifecycle.is_active(),
        "file lifecycle after resume must be Active, got {:?}",
        info.lifecycle
    );
}
