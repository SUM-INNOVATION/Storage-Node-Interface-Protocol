//! WS2b — local-mirror E2E suite.
//!
//! All tests `#[ignore]`'d. Runs via `make e2e-mirror` (manual gate,
//! never in `release-check` or PR CI). Drives the production
//! `sum-node` and `e2e-helper` binaries as subprocesses against a
//! real running chain mirror at `http://localhost:8545`.
//!
//! Preconditions checked at runtime:
//!   * Mirror up + V2 enabled (chain_id 1337, finality advancing).
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
//!  13. reassignment_refetch_restores_public_coverage — opt-in with
//!      `SNIP_E2E_REASSIGNMENT=1` on a devnet with archive reassignment
//!      enabled (the pinned synthetic devnet keeps it dormant)
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
    E2E_FINALITY_BUDGET_SECS, await_tx_finality, e2e_rpc_url, funded_or_skip,
    mirror_running_or_skip, role_address, role_seed_path, spawn_bin,
};

// ── Shared per-test setup ───────────────────────────────────────────────────

/// Poll `probe` once a second until it yields `Ok(v)`, for at most `budget`,
/// then panic with `what` and the last observation. The deterministic
/// replacement for "sleep a fixed time, then assert": it returns as soon as
/// the chain shows the expected state, and fails loudly with what it saw.
async fn wait_for<T, F, Fut>(what: &str, budget: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match probe().await {
            Ok(v) => return v,
            Err(last) => assert!(
                tokio::time::Instant::now() < deadline,
                "e2e: {what} not observed within {budget:?} (last: {last})"
            ),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// How long a CLI command's chain effect may take to become visible when the
/// command does not print a tx hash to wait on: twice the per-tx finality
/// budget.
fn chain_effect_budget() -> Duration {
    Duration::from_secs(2 * E2E_FINALITY_BUDGET_SECS)
}

/// Wait until the chain height reaches `h`.
async fn wait_height_at_least(rpc: &L1RpcClient, h: u64, what: &str) -> u64 {
    // From any height the chain needs (h - now) blocks; budget generously
    // per block, bounded.
    let budget = Duration::from_secs(60) + chain_effect_budget();
    wait_for(what, budget, move || async move {
        match rpc.chain_get_block_height().await {
            Ok(head) if head.height >= h => Ok(head.height),
            Ok(head) => Err(format!("height {} < {h}", head.height)),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
}

/// What a test needs the chain's access list to show for one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessShows {
    /// An entry for the address exists.
    Present,
    /// No entry for the address.
    Absent,
    /// An entry with exactly this expiry (`None` = no expiry).
    Expiry(Option<u64>),
}

/// Wait until `storage_getFileInfoV2` shows `want` for `addr` on `root_hex`.
async fn wait_access(rpc: &L1RpcClient, root_hex: &str, addr: &str, want: AccessShows) {
    let what = format!("access list of {root_hex} showing {want:?} for {addr}");
    wait_for(&what, chain_effect_budget(), move || async move {
        let info = rpc
            .storage_get_file_info_v2(root_hex, None, None)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "file not found".to_string())?;
        let entry = info.access_list.iter().find(|e| e.address == addr);
        let ok = match (want, entry) {
            (AccessShows::Present, Some(_)) => true,
            (AccessShows::Absent, None) => true,
            (AccessShows::Expiry(x), Some(e)) => e.expires_at == x,
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(format!("entry {entry:?}"))
        }
    })
    .await
}

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
        "1337".into(),
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

/// Argument vector for one archive's `sum-node listen`.
///
/// Split out from [`spawn_archive_listen`] so the isolation property is
/// testable without a chain: the root each archive is given appears in its
/// own argv, and two archives are given two different roots.
fn archive_listen_args(role: &str, store_dir: &std::path::Path) -> Vec<String> {
    archive_listen_args_with(&role_seed_path(role), store_dir, &[])
}

/// [`archive_listen_args`] for any key file, with extra global flags placed
/// before the `listen` subcommand (for example `--refetch-secs 5`).
fn archive_listen_args_with(
    key_file: &std::path::Path,
    store_dir: &std::path::Path,
    extra: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "--key-file".to_string(),
        key_file.to_str().unwrap().to_string(),
        // Each archive needs its own store root so they do not share one
        // chunk namespace. This must be said explicitly: store-root
        // resolution consults the `--store-dir` flag, then `$SUM_STORE_DIR`,
        // then `$HOME` — never the working directory. This call site
        // previously handed the root to `Command::current_dir`, which
        // resolution does not read, so all three archives resolved to the
        // same root and "the chunks are on disk" held for reasons unrelated
        // to replication. (`env_clear` also strips `HOME`, so since the
        // fail-closed change an archive started this way would not resolve a
        // root at all — it would exit asking for `--store-dir`.)
        "--store-dir".to_string(),
        store_dir.display().to_string(),
        "--rpc-url".to_string(),
        e2e_rpc_url(),
        "--chain-id".to_string(),
        "1337".to_string(),
    ];
    args.extend(extra.iter().map(|a| a.to_string()));
    args.push("listen".to_string());
    args
}

/// Spawn a long-running `sum-node listen` archive node. Returns
/// the `Child` handle so the test can kill it during teardown.
/// Callers are responsible for waiting a few seconds before the
/// archive is fully discoverable (libp2p mDNS / chain registration).
fn spawn_archive_listen(role: &str, store_dir: &std::path::Path) -> std::process::Child {
    spawn_listen_with_args(role, archive_listen_args(role, store_dir))
}

/// Spawn `sum-node` with `args` (a `listen` invocation); `label` names it in
/// panics.
fn spawn_listen_with_args(label: &str, args: Vec<String>) -> std::process::Child {
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
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn()
        .unwrap_or_else(|e| panic!("e2e: spawn sum-node listen for {label}: {e}"))
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
            "1337",
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
            "1337",
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
    // The CLI doesn't always return the tx hash on stdout, so wait on the
    // chain state itself (bounded) rather than a fixed sleep.
    let addr = role_address("owner");
    let (rpc_ref, addr_ref) = (&rpc, addr.as_str());
    let pk = wait_for(
        &format!("an encryption pubkey for owner ({addr})"),
        chain_effect_budget(),
        move || async move {
            match rpc_ref.account_get_encryption_public_key(addr_ref).await {
                Ok(Some(pk)) => Ok(Some(pk)),
                Ok(None) => Err("account_getEncryptionPublicKey returned None".into()),
                Err(e) => Err(format!("account_getEncryptionPublicKey: {e}")),
            }
        },
    )
    .await;
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
            "1337",
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
            "1337",
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
            "1337",
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
            "1337",
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
    wait_access(&rpc, &root_hex, &third_party_addr, AccessShows::Present).await;

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
            "1337",
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
            "1337",
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
            "1337",
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
    // RemoveAccessV2 removes the entry: wait until the chain shows it gone
    // (bounded), so the refusal below is tested against a finalized revoke
    // rather than raced against a fixed sleep.
    wait_access(&rpc, &root_hex, &recipient_addr, AccessShows::Absent).await;

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
            "1337",
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

    // Wait until the chain is past the original expiry (by height, with a
    // two-block margin), instead of a fixed sleep.
    wait_height_at_least(
        &rpc,
        original_expires + 2,
        "the chain past the original expiry",
    )
    .await;

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
            "1337",
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
    wait_access(
        &rpc,
        &root_hex,
        &recipient_addr,
        AccessShows::Expiry(Some(new_expires)),
    )
    .await;

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
            "1337",
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
            "1337",
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
    // The clear is on chain, and the chain is past the ORIGINAL expiry (by
    // height, two-block margin), which is what proves the clear took effect.
    wait_access(&rpc, &root_hex, &recipient_addr, AccessShows::Expiry(None)).await;
    wait_height_at_least(
        &rpc,
        original_expires + 2,
        "the chain past the original expiry",
    )
    .await;

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
            "1337",
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

/// No archives listening → refused before registration; Pending file →
/// resume → activated.
///
/// Phase 0: with no archive listening, `ingest-v2` must refuse before
/// `RegisterFilePendingV2`. The chain lists three active archives the
/// file would be assigned to, none is identified within the discovery
/// bound, and no row may exist for the file afterwards.
///
/// Phase 1 forces a `Pending` outcome deterministically on a healthy
/// fleet: `--push-wait-secs 0` ends S2 right after the initial push
/// wave, before any `PushAck` is read, so every chunk is
/// under-replicated and the file stays `Pending` after registration.
/// (Before discovery waited for every assigned archive, this phase ran
/// with no archives listening; that now refuses before registering.)
/// Phase 2: `resume` against the same root + file body must drive the
/// file to `Active`.
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
    let (_m, manifest) = sum_store::chunker::BinaryChunker::chunk_file(&pt_path)
        .expect("chunk the plaintext locally to learn its merkle root");
    let expected_root = format!("0x{}", hex::encode(manifest.merkle_root));

    // Phase 0 — no archive listeners: refused before registration.
    let refused = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            owner_key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "1337",
            "ingest-v2",
            pt_path.to_str().unwrap(),
            "--visibility",
            "public",
        ],
        &[],
    );
    let combined_refused = format!(
        "{}\n{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        combined_refused.contains("Refusing to register the file"),
        "ingest-v2 with no archives listening must refuse before registration: \
         {combined_refused}"
    );
    assert!(
        !combined_refused.contains("lifecycle: Pending"),
        "a refused ingest must not report a Pending file: {combined_refused}"
    );
    assert!(
        rpc.storage_get_file_info_v2(&expected_root, None, None)
            .await
            .expect("storage_getFileInfoV2 after refused ingest")
            .is_none(),
        "a refused ingest must leave no chain row for {expected_root}"
    );

    // Phase 1 — fleet up; induce Pending with a zero push wait. The CLI
    // exit code for Pending is not asserted on.
    let (_archives, _archive_dirs) = spawn_archive_fleet(&rpc).await;
    let initial = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            owner_key.to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "--chain-id",
            "1337",
            "ingest-v2",
            pt_path.to_str().unwrap(),
            "--visibility",
            "public",
            "--push-wait-secs",
            "0",
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
    assert_eq!(
        prefixed, expected_root,
        "the registered root is the local one"
    );
    assert!(
        combined_initial.contains("lifecycle: Pending"),
        "phase 1 must leave the file Pending: {combined_initial}"
    );

    // Phase 2 — resume with the fleet still up. Discovery waits for the
    // archives assigned to the missing chunks; S2 then reaches R=3.

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
            "1337",
            "resume",
            &prefixed,
            pt_path.to_str().unwrap(),
        ],
        &[],
    );
    // Exit 0 means the file is Active (ActivateFileV2 finalized, or already
    // Active); 3 still Pending, 4 Abandoned, 5 root mismatch, 1 error.
    assert!(
        resume_out.status.success(),
        "resume did not complete activation (exit {:?}: 3 Pending, 4 Abandoned, \
         5 root mismatch, 1 error): stderr={}\nstdout={}",
        resume_out.status.code(),
        String::from_utf8_lossy(&resume_out.stderr),
        String::from_utf8_lossy(&resume_out.stdout),
    );

    // With exit 0, resume also prints `lifecycle: Active`, which it does
    // only after `ActivateFileV2` has finalized (or when the chain already
    // shows the file Active), so the chain row is Active by the time it
    // returns. Both are required.
    let resume_stdout = String::from_utf8_lossy(&resume_out.stdout).to_string();
    let resume_stderr = String::from_utf8_lossy(&resume_out.stderr).to_string();
    assert!(
        resume_stdout
            .lines()
            .any(|l| l.trim() == "lifecycle: Active"),
        "resume did not report an activated file: stdout={resume_stdout}\nstderr={resume_stderr}"
    );

    // Confirm on chain state: poll until Active, bounded; Abandoned is
    // terminal.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let info = rpc
            .storage_get_file_info_v2(&prefixed, None, None)
            .await
            .expect("storage_getFileInfoV2 after resume")
            .expect("V2 row must be present after resume");
        if info.lifecycle.is_active() {
            break;
        }
        assert!(
            !info.lifecycle.is_abandoned(),
            "the file is Abandoned after resume reported Active: stdout={resume_stdout}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "chain row not Active 30s after resume reported Active (got {:?}): \
             stdout={resume_stdout}\nstderr={resume_stderr}",
            info.lifecycle
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── 13. reassignment_refetch_restores_public_coverage ───────────────────────

/// Opt-in switch for scenario 13. The pinned synthetic devnet keeps
/// `archive_reassignment_enabled_from_height` unset (sum-chain
/// `scripts/src/setup_local_testnet.rs`), so `ReassignChunksV2` cannot execute
/// there and the scenario would fail at step 5. It runs only with this set to
/// `1`, against a devnet where that gate is set.
const REASSIGNMENT_OPT_IN: &str = "SNIP_E2E_REASSIGNMENT";

/// Re-fetch and a short epoch watch, for archives in scenario 13.
const REFETCH_FLAGS: [&str; 4] = ["--refetch-secs", "5", "--epoch-watch-secs", "5"];

/// Build, sign and submit one V2 transaction from `seed`, then wait for
/// finality. Synthetic devnet keys only.
async fn submit_v2(
    rpc: &L1RpcClient,
    seed: &[u8; 32],
    payload: sumchain_wire::transaction::TxPayload,
) -> u64 {
    let pubkey = ed25519_dalek::SigningKey::from_bytes(seed)
        .verifying_key()
        .to_bytes();
    let from = sumchain_wire::Address::from_public_key(&pubkey);
    let nonce = rpc
        .get_nonce(&from.to_base58())
        .await
        .expect("e2e: get_nonce");
    let fee = rpc
        .chain_get_chain_params()
        .await
        .expect("e2e: chain_getChainParams")
        .min_fee;
    let tx = sumchain_wire::transaction::TransactionV2 {
        chain_id: 1337,
        from,
        fee,
        nonce,
        payload,
    };
    let hex = sum_node::tx_builder::sign_transaction_v2(seed, tx).expect("e2e: sign");
    let hash = rpc.send_raw_transaction(&hex).await.expect("e2e: submit");
    await_tx_finality(rpc, &hash).await
}

/// Wait until the finalized active-node snapshot shows `n` archives.
async fn wait_active_archives(rpc: &L1RpcClient, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let head = rpc.chain_get_block_height().await.expect("e2e: height");
        let nodes = rpc
            .storage_get_active_nodes_at_height(head.height)
            .await
            .expect("e2e: active nodes");
        let archives = nodes.iter().filter(|x| x.role == "ArchiveNode").count();
        if archives >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "e2e: {archives} active archives after 60s, expected {n}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Spawn `sum-node` (a `listen` invocation) with stdout and stderr appended
/// to `log`, at `RUST_LOG=info`. A file rather than a pipe: a long run never
/// blocks on an undrained pipe, and the scenario reads the log back.
fn spawn_listen_logged(
    label: &str,
    args: Vec<String>,
    log: &std::path::Path,
) -> std::process::Child {
    use std::process::Stdio;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap_or_else(|e| panic!("e2e: open the log for {label}: {e}"));
    let err = out.try_clone().expect("e2e: clone the log handle");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_sum-node"));
    cmd.env_clear();
    cmd.env("RUST_LOG", "info");
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(args);
    cmd.stdout(Stdio::from(out));
    cmd.stderr(Stdio::from(err));
    cmd.spawn()
        .unwrap_or_else(|e| panic!("e2e: spawn sum-node listen for {label}: {e}"))
}

/// A log file's text with ANSI escape sequences removed.
fn read_log_plain(log: &std::path::Path) -> String {
    let raw = std::fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Lines of `log` that record a corrupt-bytes refusal of `index` from `from`.
fn corrupt_refusals(log: &str, index: u32, from: &[u8; 20]) -> usize {
    let index_field = format!("index={index}");
    let from_field = format!("from={}", hex::encode(from));
    log.lines()
        .filter(|l| {
            l.contains("re-fetch: bytes do not match the manifest; refused")
                && l.split_whitespace().any(|w| w == index_field)
                && l.contains(&from_field)
        })
        .count()
}

/// The `planned=` count of the first completed re-fetch run in `log`.
fn first_run_planned(log: &str) -> Option<u64> {
    let line = log.lines().find(|l| l.contains("re-fetch: run complete"))?;
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("planned="))
        .and_then(|v| v.parse().ok())
}

/// The re-fetch plan the runner computes for `me` on `root`, from the same
/// committed inputs it reads: the coverage row's assignment epochs (at most
/// the last `MAX_EPOCHS_CONSULTED`), the active-archive snapshot at each, and
/// R from the chain's parameters, with nothing held yet.
async fn refetch_plan_for(
    rpc: &L1RpcClient,
    root: &[u8; 32],
    chunk_count: u32,
    me: &[u8; 20],
) -> Vec<sum_node::refetch_v2::ChunkPlan> {
    use sum_node::inbound_v2::AttestTriggerRpc;
    let cov = AttestTriggerRpc::fetch_coverage(rpc, root)
        .await
        .expect("e2e: coverage")
        .expect("e2e: coverage row");
    let heights = &cov.assignment_epochs;
    let from = heights
        .len()
        .saturating_sub(sum_node::refetch_v2::MAX_EPOCHS_CONSULTED);
    let mut epochs = Vec::new();
    for &h in &heights[from..] {
        let snapshot = AttestTriggerRpc::fetch_snapshot(rpc, h)
            .await
            .expect("e2e: snapshot");
        epochs.push((h, snapshot));
    }
    let params = rpc
        .chain_get_chain_params()
        .await
        .expect("e2e: chain_getChainParams");
    let r = sum_node::runtime_params::RuntimeChainParams::from_chain(&params)
        .assignment_replication_factor;
    sum_node::refetch_v2::plan_refetch(
        root,
        chunk_count,
        &epochs,
        r,
        me,
        &std::collections::BTreeSet::new(),
    )
}

/// Every `<cid>.chunk` file under `dir`, by CID.
fn stored_chunks(dir: &std::path::Path) -> std::collections::BTreeMap<String, PathBuf> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(cid) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".chunk"))
            {
                out.insert(cid.to_string(), p);
            }
        }
    }
    out
}

/// Replace one stored chunk file of a devnet test archive with a same-length
/// copy whose first byte is flipped (written beside it, then renamed over
/// it), and return the original bytes.
fn corrupt_stored_chunk(store_dir: &std::path::Path, cid: &str) -> Vec<u8> {
    let path = stored_chunks(store_dir).remove(cid).unwrap_or_else(|| {
        panic!(
            "e2e: chunk {cid} is not stored under {}; the archive did not receive it",
            store_dir.display()
        )
    });
    let original = std::fs::read(&path).expect("e2e: read the stored chunk");
    let mut bad = original.clone();
    bad[0] ^= 0xFF;
    replace_file(&path, &bad);
    original
}

/// Write `bytes` beside `path` and rename the copy over it.
fn replace_file(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("e2e-replace.tmp");
    std::fs::write(&tmp, bytes).expect("e2e: write the replacement");
    std::fs::rename(&tmp, path).expect("e2e: rename the replacement into place");
}

/// Latest-epoch coverage of `root`, and the fourth archive's entry in it.
async fn latest_epoch_of(
    rpc: &L1RpcClient,
    root_hex: &str,
    archive: &str,
) -> (
    sum_types::rpc_types::AssignmentCoverageV2,
    Option<sum_types::rpc_types::ArchiveCoverageSummaryV2>,
) {
    let cov = rpc
        .storage_get_assignment_coverage_v2(root_hex, None, None)
        .await
        .expect("e2e: coverage")
        .expect("e2e: coverage row");
    let entry = cov
        .per_epoch
        .last()
        .and_then(|e| e.per_archive.iter().find(|a| a.archive == archive).cloned());
    (cov, entry)
}

/// G3 P3 end to end, on a devnet whose genesis opens archive unbonding and
/// archive reassignment (and nothing else): an assigned archive leaves the
/// active set by its own BeginUnstake, the owner reassigns, and a fourth
/// archive that the reassignment newly assigns
/// - re-fetches the Public file's chunks from the surviving archives,
///   refusing bytes that do not match the manifest and moving to the next
///   source;
/// - is restarted while one chunk is still missing, keeps what it already
///   holds, and fetches only that chunk afterwards;
/// - attests in the latest epoch, so coverage is restored and the chain no
///   longer reports a reassignment as needed;
/// - never fetches, stores or attests a Private file it is newly assigned.
#[tokio::test]
#[ignore]
async fn reassignment_refetch_restores_public_coverage() {
    if std::env::var(REASSIGNMENT_OPT_IN).as_deref() != Ok("1") {
        eprintln!(
            "skipped: set {REASSIGNMENT_OPT_IN}=1 to run this against a devnet with \
             archive_reassignment_enabled_from_height and \
             archive_unbonding_enabled_from_height set; the pinned synthetic devnet \
             keeps both dormant"
        );
        return;
    }
    let rpc = setup().await;
    funded_or_skip(&rpc, "owner").await;
    funded_or_skip(&rpc, "third_party").await;
    ensure_encryption_key_registered(&rpc, "owner").await;
    let logs = tempfile::tempdir().expect("e2e: log dir");

    // 1. Three archives with re-fetch on, each logging to a file.
    for role in ARCHIVE_ROLES {
        funded_or_skip(&rpc, role).await;
        ensure_archive_registered(&rpc, role).await;
    }
    let mut guards: Vec<Option<ChildGuard>> = Vec::new();
    let mut dirs = Vec::new();
    for role in ARCHIVE_ROLES {
        let dir = tempfile::tempdir().expect("e2e: archive store dir");
        let args = archive_listen_args_with(&role_seed_path(role), dir.path(), &REFETCH_FLAGS);
        let log = logs.path().join(format!("{role}.log"));
        guards.push(Some(ChildGuard::new(spawn_listen_logged(role, args, &log))));
        dirs.push(dir);
    }
    wait_active_archives(&rpc, ARCHIVE_ROLES.len()).await;
    // The chain side is waited on above; this short settle covers libp2p
    // (mDNS) discoverability only, which has no observable state to wait on
    // from here (as in `spawn_archive_fleet`).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // 2. A four-chunk Public file and a one-chunk owner-only Private file,
    //    both assigned over these three archives.
    let public_bytes = unique_plaintext("reassignment_refetch", 3 * 1024 * 1024 + 300 * 1024);
    let (_public_dir, public_path) = write_test_plaintext(&public_bytes);
    let root = spawn_ingest_and_extract_root(&role_seed_path("owner"), &public_path, "public", &[]);
    let root_hex = format!("0x{}", hex::encode(root));
    let info = rpc
        .storage_get_file_info_v2(&root_hex, None, None)
        .await
        .expect("e2e: file info")
        .expect("e2e: file registered");
    assert_eq!(
        info.chunk_count, 4,
        "e2e: the Public file must span four chunks"
    );
    let (_mmap, manifest) =
        sum_store::BinaryChunker::chunk_file(&public_path).expect("e2e: chunk the Public file");
    let public_cids: Vec<String> = manifest.chunks.iter().map(|c| c.cid.clone()).collect();

    let private_bytes = unique_plaintext("reassignment_refetch_private", 2048);
    let (_private_dir, private_path) = write_test_plaintext(&private_bytes);
    let private_root =
        spawn_ingest_and_extract_root(&role_seed_path("owner"), &private_path, "private", &[]);
    let private_root_hex = format!("0x{}", hex::encode(private_root));

    // 3. A fourth archive on a fresh throwaway key, funded inside the devnet
    //    by `third_party` and registered after both files' assignment
    //    heights, so only the reassignment assigns it. Its listener starts
    //    later, once the sources are prepared.
    let seed4: [u8; 32] = {
        use rand_core::{OsRng, RngCore};
        let mut s = [0u8; 32];
        OsRng.fill_bytes(&mut s);
        s
    };
    let key4 = common::write_seed_tempfile(&seed4);
    let pub4 = ed25519_dalek::SigningKey::from_bytes(&seed4)
        .verifying_key()
        .to_bytes();
    let addr4 = sumchain_wire::Address::from_public_key(&pub4);
    let addr4_b58 = addr4.to_base58();
    let me4 = sum_net::l1_address_from_base58(&addr4_b58).expect("e2e: decode address");
    let stake = rpc
        .get_node_record(&role_address("archive_1"))
        .await
        .expect("e2e: node record")
        .expect("e2e: archive_1 registered")
        .staked_balance;
    submit_v2(
        &rpc,
        &common::read_e2e_seed("third_party"),
        sumchain_wire::transaction::TxPayload::Transfer {
            to: addr4,
            amount: u128::from(stake) * 2 + 10_000_000_000,
        },
    )
    .await;
    let out = spawn_bin(
        "sum-node",
        &[
            "--key-file",
            key4.path().to_str().unwrap(),
            "--rpc-url",
            &e2e_rpc_url(),
            "register-node",
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "e2e: register the fourth archive: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_active_archives(&rpc, ARCHIVE_ROLES.len() + 1).await;

    // 4. archive_3 leaves the active set by its own full-stake BeginUnstake,
    //    and its listener stops, so it is no longer a reachable source.
    let staked3 = rpc
        .get_node_record(&role_address("archive_3"))
        .await
        .expect("e2e: node record")
        .expect("e2e: archive_3 registered")
        .staked_balance;
    submit_v2(
        &rpc,
        &common::read_e2e_seed("archive_3"),
        sumchain_wire::transaction::TxPayload::NodeRegistry(
            sumchain_wire::node_registry::NodeRegistryTxData {
                operation: sumchain_wire::node_registry::NodeRegistryOperation::BeginUnstake {
                    amount: staked3,
                },
            },
        ),
    )
    .await;
    guards[2].take();

    // 5. The owner reassigns both files once the chain reports it needed.
    for target in [&root_hex, &private_root_hex] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let cov = rpc
                .storage_get_assignment_coverage_v2(target, None, None)
                .await
                .expect("e2e: coverage")
                .expect("e2e: coverage row");
            if cov.reassignment_needed {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "e2e: the chain never reported reassignment_needed for {target} after \
                 archive_3 left; are archive unbonding and reassignment open on this devnet?"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let out = spawn_bin(
            "sum-node",
            &[
                "--key-file",
                role_seed_path("owner").to_str().unwrap(),
                "--rpc-url",
                &e2e_rpc_url(),
                "--chain-id",
                "1337",
                "reassign",
                target,
            ],
            &[],
        );
        assert!(
            out.status.success(),
            "e2e: sum-node reassign {target} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 6. Prepare the sources, inside this devnet only. The plan is the one the
    //    fourth archive's runner computes: every chunk is new to it.
    let plan = refetch_plan_for(&rpc, &root, info.chunk_count, &me4).await;
    assert_eq!(
        plan.len() as u32,
        info.chunk_count,
        "e2e: the fourth archive should be newly assigned every chunk: {plan:?}"
    );
    let survivor_dir = |a: &[u8; 20]| -> &std::path::Path {
        for (i, role) in ["archive_1", "archive_2"].iter().enumerate() {
            let addr = sum_net::l1_address_from_base58(&role_address(role)).unwrap();
            if addr == *a {
                return dirs[i].path();
            }
        }
        panic!("e2e: source {} is not a surviving archive", hex::encode(a));
    };
    let (x, y) = (&plan[0], &plan[1]);
    let survivors_x: Vec<[u8; 20]> = x.sources.iter().take(2).copied().collect();
    let cid_x = public_cids[x.chunk_index as usize].clone();
    let cid_y = public_cids[y.chunk_index as usize].clone();
    // Chunk X: corrupt on both surviving sources, so the first run cannot
    // fetch it from anyone (archive_3 is gone).
    let originals_x: Vec<([u8; 20], Vec<u8>)> = survivors_x
        .iter()
        .map(|s| (*s, corrupt_stored_chunk(survivor_dir(s), &cid_x)))
        .collect();
    // Chunk Y: corrupt on its first source only, so the next source is used.
    let first_y = y.sources[0];
    let _original_y = corrupt_stored_chunk(survivor_dir(&first_y), &cid_y);

    // 7. Run 1: the fourth archive fetches and attests everything except X.
    let dir4 = tempfile::tempdir().expect("e2e: fourth archive store dir");
    let run1 = logs.path().join("archive_4.run1.log");
    let args4 = archive_listen_args_with(key4.path(), dir4.path(), &REFETCH_FLAGS);
    let mut archive4 = ChildGuard::new(spawn_listen_logged("archive_4", args4.clone(), &run1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let (_, fourth) = latest_epoch_of(&rpc, &root_hex, &addr4_b58).await;
        let attested = fourth.as_ref().map_or(0, |a| a.attested_count);
        let log = read_log_plain(&run1);
        let x_refused = survivors_x
            .iter()
            .all(|s| corrupt_refusals(&log, x.chunk_index, s) > 0);
        if attested == info.chunk_count - 1 && x_refused {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "e2e: run 1 did not reach {} attested chunks with X refused from both survivors \
             after 240s: attested {attested}, fourth {fourth:?}\n--- run 1 log tail ---\n{}",
            info.chunk_count - 1,
            log.lines().rev().take(60).collect::<Vec<_>>().join("\n")
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // X stays missing while every reachable copy is corrupt. This is a
    // negative check, so it is an explicit, bounded observation window, not a
    // wait-until: 12 s covers at least two re-fetch runs at the 5 s interval
    // (REFETCH_FLAGS), and the window only counts if the archive actually
    // completed another run inside it (otherwise "still missing" would prove
    // nothing).
    let runs_before = read_log_plain(&run1)
        .matches("re-fetch: run complete")
        .count();
    tokio::time::sleep(Duration::from_secs(12)).await;
    let runs_after = read_log_plain(&run1)
        .matches("re-fetch: run complete")
        .count();
    assert!(
        runs_after > runs_before,
        "e2e: no re-fetch run completed during the 12 s observation window \
         ({runs_before} before, {runs_after} after), so it observed nothing"
    );
    let (_, fourth) = latest_epoch_of(&rpc, &root_hex, &addr4_b58).await;
    assert_eq!(
        fourth.as_ref().map(|a| a.attested_count),
        Some(info.chunk_count - 1),
        "e2e: chunk X was accepted although every reachable copy was corrupt"
    );
    let run1_log = read_log_plain(&run1);

    // 8. Restart mid-way: stop the fourth archive while X is still missing,
    //    restore X on its second source only, and start it again on the same
    //    store. It must keep what it holds and fetch X alone.
    if let Some(mut c) = archive4.0.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let held_before_restart = stored_chunks(dir4.path()).len();
    assert_eq!(
        held_before_restart as u32,
        info.chunk_count - 1,
        "e2e: the fourth archive should hold every chunk but X before the restart"
    );
    let (second_x, original_second_x) = &originals_x[1];
    let path_second_x = stored_chunks(survivor_dir(second_x))
        .remove(&cid_x)
        .expect("e2e: X is stored on its second source");
    replace_file(&path_second_x, original_second_x);
    let run2 = logs.path().join("archive_4.run2.log");
    let _archive4 = ChildGuard::new(spawn_listen_logged("archive_4", args4, &run2));

    // 9. Coverage is restored under the latest epoch, the fourth archive has
    //    attested every chunk assigned to it, and no reassignment is needed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let (cov, fourth) = latest_epoch_of(&rpc, &root_hex, &addr4_b58).await;
        let latest = cov.per_epoch.last().expect("e2e: per-epoch coverage");
        let fourth_done = fourth.as_ref().is_some_and(|a| {
            a.assigned_count
                .is_some_and(|n| n == info.chunk_count && a.attested_count == n)
        });
        if latest.epoch_height == cov.latest_assignment_epoch
            && cov.assignment_epochs.len() >= 2
            && latest.covered_count == info.chunk_count
            && fourth_done
            && !cov.reassignment_needed
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "e2e: coverage not restored under the latest epoch after the restart (240s): \
             covered {}/{}, fourth {fourth:?}, reassignment_needed {}\n--- run 2 log tail ---\n{}",
            latest.covered_count,
            info.chunk_count,
            cov.reassignment_needed,
            read_log_plain(&run2)
                .lines()
                .rev()
                .take(60)
                .collect::<Vec<_>>()
                .join("\n")
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let run2_log = read_log_plain(&run2);
    assert_eq!(
        first_run_planned(&run2_log),
        Some(1),
        "e2e: after the restart the fourth archive should plan X alone (held set rebuilt \
         from disk)\n--- run 2 log ---\n{run2_log}"
    );

    // The next source was used after a corrupt first source: Y in run 1, or
    // X in run 2 (its first source is still corrupt).
    let y_next = corrupt_refusals(&run1_log, y.chunk_index, &first_y) > 0;
    let x_next = corrupt_refusals(&run2_log, x.chunk_index, &x.sources[0]) > 0;
    assert!(
        y_next || x_next,
        "e2e: no corrupt-first-source refusal followed by acceptance was observed \
         (Y in run 1 or X in run 2)\n--- run 1 ---\n{run1_log}\n--- run 2 ---\n{run2_log}"
    );
    eprintln!(
        "e2e: scenario 13 evidence: chunks {}, plan {:?}; X={} refused from both survivors in \
         run 1; Y={} next-source in run 1: {y_next}; X next-source in run 2: {x_next}; run 2 \
         planned {:?}",
        info.chunk_count,
        plan.iter().map(|c| c.chunk_index).collect::<Vec<_>>(),
        x.chunk_index,
        y.chunk_index,
        first_run_planned(&run2_log)
    );

    // 10. The Private file: newly assigned to the fourth archive, never
    //     fetched, stored or attested; the skip reason is logged.
    let private_hex_bare = hex::encode(private_root);
    assert!(
        [&run1_log, &run2_log]
            .iter()
            .any(|l| l.lines().any(|line| line
                .contains(sum_node::refetch_v2::PRIVATE_SKIP_REASON)
                && line.contains(&private_hex_bare))),
        "e2e: the Private skip reason was not logged for {private_root_hex}"
    );
    let (pcov, pfourth) = latest_epoch_of(&rpc, &private_root_hex, &addr4_b58).await;
    let pfourth = pfourth.expect("e2e: the fourth archive is in the Private file's latest epoch");
    assert!(
        pfourth.assigned_count.is_some_and(|n| n > 0),
        "e2e: the fourth archive should be newly assigned the Private file: {pfourth:?}"
    );
    assert_eq!(
        pfourth.attested_count, 0,
        "e2e: the fourth archive attested Private chunks: {pfourth:?} ({pcov:?})"
    );
    let held: std::collections::BTreeSet<String> = stored_chunks(dir4.path()).into_keys().collect();
    let expected: std::collections::BTreeSet<String> = public_cids.iter().cloned().collect();
    assert_eq!(
        held, expected,
        "e2e: the fourth archive stores exactly the Public file's chunks"
    );
    drop(guards);
    drop(dirs);
}

/// Re-fetch flags reach the archive as global flags, and the default fleet
/// runs with re-fetch off.
mod refetch_args {
    use super::{REFETCH_FLAGS, archive_listen_args, archive_listen_args_with};

    #[test]
    fn default_archives_do_not_enable_refetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = archive_listen_args("archive_1", dir.path());
        assert!(!args.iter().any(|a| a == "--refetch-secs"));
    }

    #[test]
    fn extra_flags_precede_the_listen_subcommand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("k.seed.hex");
        let args = archive_listen_args_with(&key, dir.path(), &REFETCH_FLAGS);
        let listen = args.iter().position(|a| a == "listen").expect("listen");
        assert_eq!(listen, args.len() - 1, "listen is last");
        let flag = args
            .iter()
            .position(|a| a == "--refetch-secs")
            .expect("flag");
        assert!(flag < listen);
        assert_eq!(args[flag + 1], "5");
    }
}

/// Store-root isolation for the archive fleet, provable without a chain.
///
/// Every scenario in this file is `#[ignore]`d behind a live mirror, so the
/// property that the fleet's three archives do not share a store root had no
/// unignored coverage. These two run in an ordinary `cargo test`.
mod archive_store_isolation {
    use super::{ARCHIVE_ROLES, archive_listen_args};

    /// Read back the value the argv gives to `--store-dir`.
    fn store_dir_arg(args: &[String]) -> Option<&str> {
        let at = args.iter().position(|a| a == "--store-dir")?;
        args.get(at + 1).map(String::as_str)
    }

    #[test]
    fn each_archive_is_told_its_own_store_root() {
        let dirs: Vec<_> = ARCHIVE_ROLES
            .iter()
            .map(|_| tempfile::tempdir().expect("tempdir"))
            .collect();

        let roots: Vec<String> = ARCHIVE_ROLES
            .iter()
            .zip(&dirs)
            .map(|(role, dir)| {
                let args = archive_listen_args(role, dir.path());
                store_dir_arg(&args)
                    .expect("every archive must be told a store root explicitly")
                    .to_string()
            })
            .collect();

        for (root, dir) in roots.iter().zip(&dirs) {
            assert_eq!(root.as_str(), dir.path().display().to_string());
        }

        let mut unique = roots.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            ARCHIVE_ROLES.len(),
            "the {} archives share a store root: {roots:?} — replication assertions \
             would pass on one node's chunks",
            ARCHIVE_ROLES.len()
        );
    }

    /// The working directory is not a store-root source, so it must not be
    /// the thing carrying isolation.
    #[test]
    fn the_root_travels_in_argv_not_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = archive_listen_args("archive_1", dir.path());
        assert_eq!(
            store_dir_arg(&args),
            Some(dir.path().display().to_string()).as_deref()
        );
    }
}
