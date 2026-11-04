//! `sum-node ingest-v2` exit status at the process boundary (SNIP #77).
//!
//! Without `--exit-status`, every outcome exits 0 and stdout carries the
//! same `merkle_root:` / `lifecycle:` lines as before (OmniNode's adapter
//! depends on both). With it, each outcome exits with its own code.
//!
//! The chain is an `httpmock` JSON-RPC stub scripted per outcome, and the
//! input is an empty file. An empty file has no chunks, so the pipeline
//! needs no archive on the network and the real binary can be driven to
//! each outcome without peers. How the pipeline classifies outcomes for
//! chunked files and real archive sets is covered by
//! `ingest_v2::tests::ingest_exit_status`. The usage (2) and error (1)
//! codes are checked with no chain at all, as for `resume`.

use std::path::Path;
use std::process::{Command, Output};

use httpmock::prelude::*;
use serde_json::{Value, json};
use sum_types::rpc_types::{
    AssignmentCoverageV2, BlockHeightInfo, ChainParamsInfo, LifecycleV2, NodeRecordInfo,
    StorageFileInfo, StorageFileInfoV2, TxStatusV2, VisibilityV2,
};

/// The merkle root of an empty file.
const EMPTY_ROOT: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// The chain state an outcome needs.
#[derive(Clone, Copy)]
enum Outcome {
    /// S1 and S5 finalize; coverage is complete.
    Active,
    /// S1 finalizes; coverage never completes (`--activation-wait-secs 0`).
    Pending,
    /// No eligible archive for R = 1: S1a refuses before S1, nothing sent.
    Refused,
    /// The root is already `Active` under another owner: the chain refuses
    /// the registration (receipt code 30), and the V2 row is there.
    AlreadyRegistered,
    /// The root is registered only as a V1 file: the chain refuses the
    /// registration (receipt code 30), `storage_getFileInfoV2` is `null`
    /// and `storage_getAccessList` has the row.
    AlreadyRegisteredV1,
    /// The chain refuses the registration, and the V2 lookup fails (a
    /// JSON-RPC error) while the V1 lookup finds nothing.
    LookupFails,
    /// S1 is sent and the status query fails: the outcome is unknown.
    Unconfirmed,
}

fn ok(result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "result": result})
}

fn rpc(server: &MockServer, method: &str, body: Value) {
    server.mock(|when, then| {
        when.method(POST)
            .body_contains(format!("\"method\":\"{method}\""));
        then.status(200)
            .header("content-type", "application/json")
            .json_body(body);
    });
}

fn file_row(owner: [u8; 20], lifecycle: LifecycleV2) -> StorageFileInfoV2 {
    StorageFileInfoV2 {
        merkle_root: EMPTY_ROOT.to_string(),
        owner: sum_net::identity::l1_address_base58(&owner),
        plaintext_size_bytes: 0,
        stored_size_bytes: 0,
        chunk_count: 0,
        fee_pool: 0,
        created_at: 100,
        activated_at_height: (lifecycle == LifecycleV2::ACTIVE).then_some(110),
        abandoned_at_height: None,
        assignment_height: 50,
        visibility: VisibilityV2::PUBLIC,
        lifecycle,
        access_list: vec![],
    }
}

fn coverage(can_activate_now: bool) -> AssignmentCoverageV2 {
    AssignmentCoverageV2 {
        chunk_count: 0,
        covered_count: 0,
        can_activate_now,
        missing_total: 0,
        missing_offset: 0,
        missing_indices: vec![],
        per_archive: vec![],
        assignment_epochs: vec![],
        latest_assignment_epoch: 0,
        reassignment_needed: false,
        per_epoch: vec![],
    }
}

/// Start a stub chain scripted for `outcome`.
fn chain(outcome: Outcome) -> MockServer {
    let server = MockServer::start();
    let params = ChainParamsInfo {
        chain_id: 1337,
        block_time_ms: 2000,
        finality_depth: 0,
        min_fee: 1000,
        assignment_replication_factor: 1,
        max_chunk_indices_per_tx: 65_536,
        max_chunk_count_per_file: 1_048_576,
        activation_grace_blocks: 50,
        v2_enabled_from_height: Some(0),
    };
    rpc(&server, "chain_getChainParams", ok(json!(params)));
    let head = BlockHeightInfo {
        height: 1000,
        finality: "finalized".into(),
    };
    rpc(&server, "chain_getBlockHeight", ok(json!(head)));
    rpc(&server, "get_nonce", ok(json!(1)));
    let archives: Vec<NodeRecordInfo> = match outcome {
        Outcome::Refused => vec![],
        _ => vec![NodeRecordInfo {
            address: sum_net::identity::l1_address_base58(&[0xA1; 20]),
            role: "ArchiveNode".into(),
            staked_balance: 1_000_000_000,
            status: "Active".into(),
            registered_at: 1,
        }],
    };
    rpc(
        &server,
        "storage_getActiveNodesAtHeight",
        ok(json!(archives)),
    );
    rpc(&server, "send_raw_transaction", ok(json!("0xtx")));

    let status = match outcome {
        Outcome::Active | Outcome::Pending => {
            ok(json!(TxStatusV2::Finalized { block_height: 100 }))
        }
        Outcome::AlreadyRegistered
        | Outcome::AlreadyRegisteredV1
        | Outcome::LookupFails
        | Outcome::Refused => ok(json!(TxStatusV2::Failed {
            block_height: Some(101),
            reason: "RegisterFilePendingV2 validity check failed".into(),
        })),
        Outcome::Unconfirmed => json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": "status unavailable"},
        }),
    };
    rpc(&server, "chain_getTransactionStatus", status);

    let row = match outcome {
        // Our own row, after S1 finalized.
        Outcome::Active | Outcome::Pending | Outcome::Unconfirmed => {
            ok(json!(file_row([0x01; 20], LifecycleV2::PENDING)))
        }
        Outcome::AlreadyRegistered => ok(json!(file_row([0x77; 20], LifecycleV2::ACTIVE))),
        Outcome::AlreadyRegisteredV1 | Outcome::Refused => ok(Value::Null),
        Outcome::LookupFails => json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32603, "message": "lookup unavailable"},
        }),
    };
    rpc(&server, "storage_getFileInfoV2", row);
    let v1_row = match outcome {
        Outcome::AlreadyRegisteredV1 => json!(StorageFileInfo {
            merkle_root: EMPTY_ROOT.to_string(),
            owner: sum_net::identity::l1_address_base58(&[0x77; 20]),
            total_size_bytes: 0,
            access_list: vec![],
            fee_pool: 0,
            created_at: 10,
        }),
        _ => Value::Null,
    };
    rpc(&server, "storage_getAccessList", ok(v1_row));
    rpc(
        &server,
        "storage_getAssignmentCoverageV2",
        ok(json!(coverage(matches!(outcome, Outcome::Active)))),
    );
    server
}

/// Run `sum-node` with `args` and a clean environment (a throwaway `HOME`).
fn sum_node(args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sum-node"));
    cmd.env_clear();
    cmd.env("HOME", home.path());
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(args);
    cmd.output().expect("spawn sum-node")
}

fn key_file() -> tempfile::NamedTempFile {
    let key = tempfile::NamedTempFile::new().expect("key file");
    std::fs::write(key.path(), format!("{}\n", "42".repeat(32))).expect("write key");
    key
}

/// `ingest-v2` of an empty file against `rpc_url`, with `extra` flags.
fn ingest_v2(rpc_url: &str, key: &Path, file: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "--rpc-url",
        rpc_url,
        "--key-file",
        key.to_str().unwrap(),
        "--mdns",
        "off",
        "ingest-v2",
        file.to_str().unwrap(),
        "--activation-wait-secs",
        "0",
    ];
    args.extend_from_slice(extra);
    sum_node(&args)
}

/// The machine-readable stdout lines; the rest of stdout is log output.
fn contract_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("merkle_root:") || l.starts_with("lifecycle:"))
        .map(str::to_string)
        .collect()
}

/// The contract lines for `outcome`, without (`flagged == false`) and with
/// `--exit-status`.
fn expected_lines(outcome: Outcome, flagged: bool) -> Vec<String> {
    let root = format!("merkle_root: {EMPTY_ROOT}");
    match outcome {
        Outcome::Active => vec![root, "lifecycle: Active".into()],
        Outcome::Pending => vec![root, "lifecycle: Pending".into()],
        // With the flag only: the root of a registration that may still
        // finalize, and no `lifecycle:` line.
        Outcome::Unconfirmed if flagged => vec![root],
        // No line for a root this run did not register, or may not have.
        Outcome::Refused
        | Outcome::AlreadyRegistered
        | Outcome::AlreadyRegisteredV1
        | Outcome::LookupFails
        | Outcome::Unconfirmed => vec![],
    }
}

/// One scripted outcome.
struct Case {
    outcome: Outcome,
    /// Log text showing the run reached the outcome (either mode).
    reached: &'static str,
    /// The `--exit-status` code.
    code: i32,
    /// Text that only the flagged run prints (stdout log or stderr error).
    flagged_note: &'static str,
}

const REJECTED: &str = "RegisterFilePendingV2 failed: RegisterFilePendingV2 validity check failed";
const FLAG_LOG: &str = "ingest-v2 --exit-status:";

const CASES: [Case; 7] = [
    Case {
        outcome: Outcome::Active,
        reached: "V2 ingest ACTIVATED",
        code: 0,
        flagged_note: "",
    },
    Case {
        outcome: Outcome::Pending,
        reached: "V2 ingest PENDING",
        code: 3,
        flagged_note: "",
    },
    Case {
        outcome: Outcome::Refused,
        reached: "Refusing to register it",
        code: 6,
        flagged_note: FLAG_LOG,
    },
    Case {
        outcome: Outcome::AlreadyRegistered,
        reached: REJECTED,
        code: 7,
        flagged_note: FLAG_LOG,
    },
    Case {
        outcome: Outcome::AlreadyRegisteredV1,
        reached: REJECTED,
        code: 7,
        flagged_note: FLAG_LOG,
    },
    Case {
        outcome: Outcome::LookupFails,
        reached: REJECTED,
        code: 1,
        flagged_note: "could not look the merkle root up on chain",
    },
    Case {
        outcome: Outcome::Unconfirmed,
        reached: "status unavailable",
        code: 8,
        flagged_note: FLAG_LOG,
    },
];

fn log_text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Without the flag: exit 0 and today's stdout lines, for every outcome.
#[test]
fn without_the_flag_every_outcome_exits_zero_with_the_same_stdout_lines() {
    let key = key_file();
    let file = tempfile::NamedTempFile::new().expect("empty input");
    for case in CASES {
        let server = chain(case.outcome);
        let out = ingest_v2(&server.base_url(), key.path(), file.path(), &[]);
        let log = log_text(&out);
        let name = case.reached;
        assert!(log.contains(name), "{name}: {out:?}");
        assert_eq!(out.status.code(), Some(0), "{name}: {out:?}");
        assert_eq!(
            contract_lines(&out),
            expected_lines(case.outcome, false),
            "{name}"
        );
        assert!(
            !log.contains("--exit-status"),
            "{name}: no flag, no flag log"
        );
    }
}

/// With `--exit-status`: one code per outcome, and the flagged stdout lines.
#[test]
fn with_the_flag_each_outcome_exits_with_its_own_code() {
    let key = key_file();
    let file = tempfile::NamedTempFile::new().expect("empty input");
    for case in CASES {
        let server = chain(case.outcome);
        let out = ingest_v2(
            &server.base_url(),
            key.path(),
            file.path(),
            &["--exit-status"],
        );
        let log = log_text(&out);
        let name = case.reached;
        assert!(log.contains(name), "{name}: {out:?}");
        assert_eq!(out.status.code(), Some(case.code), "{name}: {out:?}");
        assert_eq!(
            contract_lines(&out),
            expected_lines(case.outcome, true),
            "{name}"
        );
        if case.flagged_note.is_empty() {
            assert!(!log.contains("--exit-status"), "{name}");
        } else {
            assert!(log.contains(case.flagged_note), "{name}: {out:?}");
        }
    }
}

/// A failed lookup is an error under the flag: exit 1, the error on stderr
/// naming the lookup, and no claim that nothing was registered.
#[test]
fn a_failed_lookup_is_an_error_with_the_flag() {
    let key = key_file();
    let file = tempfile::NamedTempFile::new().expect("empty input");
    let server = chain(Outcome::LookupFails);
    let out = ingest_v2(
        &server.base_url(),
        key.path(),
        file.path(),
        &["--exit-status"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not look the merkle root up on chain"),
        "{stderr}"
    );
    assert!(stderr.contains("storage_getFileInfoV2"), "{stderr}");
    assert!(stderr.contains("lookup unavailable"), "{stderr}");
    let log = log_text(&out);
    assert!(
        !log.contains("registered nothing"),
        "no refusal claim: {log}"
    );
    assert!(contract_lines(&out).is_empty());
}

/// Code 8: with the flag, stdout carries the root and no `lifecycle:`
/// line; without it, neither line, as before the flag existed.
#[test]
fn an_unconfirmed_registration_prints_its_root_only_with_the_flag() {
    let key = key_file();
    let file = tempfile::NamedTempFile::new().expect("empty input");
    let root = format!("merkle_root: {EMPTY_ROOT}");

    let server = chain(Outcome::Unconfirmed);
    let flagged = ingest_v2(
        &server.base_url(),
        key.path(),
        file.path(),
        &["--exit-status"],
    );
    assert_eq!(flagged.status.code(), Some(8), "{flagged:?}");
    assert_eq!(contract_lines(&flagged), vec![root.clone()]);
    let stdout = String::from_utf8_lossy(&flagged.stdout);
    assert_eq!(stdout.lines().filter(|l| *l == root).count(), 1, "{stdout}");
    assert!(
        !stdout.lines().any(|l| l.starts_with("lifecycle:")),
        "{stdout}"
    );

    let server = chain(Outcome::Unconfirmed);
    let default = ingest_v2(&server.base_url(), key.path(), file.path(), &[]);
    assert_eq!(default.status.code(), Some(0), "{default:?}");
    assert!(contract_lines(&default).is_empty(), "{default:?}");
}

#[test]
fn a_usage_error_exits_two() {
    for args in [
        &["ingest-v2", "--exit-status"][..],
        &["ingest-v2", "payload.bin", "--exit-status=yes"][..],
    ] {
        let out = sum_node(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(contract_lines(&out).is_empty());
    }
}

/// An error before any outcome exits 1, with or without the flag (as it
/// did before the flag existed).
#[test]
fn an_error_before_any_outcome_exits_one() {
    let file = tempfile::NamedTempFile::new().expect("payload");
    let path = file.path().to_str().unwrap();
    for extra in [&[][..], &["--exit-status"][..]] {
        // No --key-file.
        let mut args = vec!["--rpc-url", "http://127.0.0.1:9", "ingest-v2", path];
        args.extend_from_slice(extra);
        let out = sum_node(&args);
        assert_eq!(out.status.code(), Some(1), "{extra:?}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--key-file"));
        assert!(contract_lines(&out).is_empty());

        // No chain: the production profile refuses without chain parameters.
        let key = key_file();
        let mut args = vec![
            "--rpc-url",
            "http://127.0.0.1:9",
            "--key-file",
            key.path().to_str().unwrap(),
            "ingest-v2",
            path,
        ];
        args.extend_from_slice(extra);
        let out = sum_node(&args);
        assert_eq!(out.status.code(), Some(1), "{extra:?}: {out:?}");
        assert!(contract_lines(&out).is_empty());
    }
}
