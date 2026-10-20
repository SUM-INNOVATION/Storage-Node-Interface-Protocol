//! `sum-node resume` exit status at the process boundary, for the outcomes
//! reachable without a chain: a usage error (the argument parser's `2`) and
//! errors before any outcome (`1`). The outcome codes themselves (`0` Active,
//! `3` Pending, `4` Abandoned, `5` root mismatch, `1` failure) are covered by
//! `ingest_v2::resume_exit_tests`, and `0` end to end by the local-mirror
//! suite's `resume_pending_completes_to_active`.

use std::process::{Command, Output};

/// Run `sum-node` with `args` and a clean environment (a throwaway `HOME`,
/// an RPC URL nothing listens on). Never reaches a chain.
fn sum_node(args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sum-node"));
    cmd.env_clear();
    cmd.env("HOME", home.path());
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(["--rpc-url", "http://127.0.0.1:9"]);
    cmd.args(args);
    cmd.output().expect("spawn sum-node")
}

fn no_lifecycle_line(out: &Output) -> bool {
    !String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim_start().starts_with("lifecycle:"))
}

#[test]
fn a_usage_error_exits_two() {
    let out = sum_node(&["resume"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(no_lifecycle_line(&out));
}

#[test]
fn a_missing_key_file_is_an_error_before_any_outcome() {
    let file = tempfile::NamedTempFile::new().expect("payload");
    let root = format!("0x{}", "11".repeat(32));
    let out = sum_node(&["resume", &root, file.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--key-file"));
    assert!(no_lifecycle_line(&out));
}

#[test]
fn an_invalid_merkle_root_is_an_error_before_any_outcome() {
    let key = tempfile::NamedTempFile::new().expect("key file");
    std::fs::write(key.path(), format!("{}\n", "42".repeat(32))).expect("write key");
    let file = tempfile::NamedTempFile::new().expect("payload");
    let out = sum_node(&[
        "--key-file",
        key.path().to_str().unwrap(),
        "resume",
        "not-hex",
        file.path().to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("merkle_root"));
    assert!(no_lifecycle_line(&out));
}
