//! E2E test helper binary for interacting with the SUM Chain L1.
//!
//! Provides CLI commands for registering nodes, registering files,
//! querying balances/blocks, checking health — all using the same
//! `L1RpcClient` and `tx_builder` as the main `sum-node` binary.
//!
//! ## Smoke posture (WS3)
//!
//! The `smoke` subcommand is the operator-facing read-only chain
//! probe. It reports `v2_enabled_from_height` per the three-state
//! `Option<u64>` distinction documented in
//! `docs/CHAIN-COMPAT.md` (`Some(0)` ≠ `Some(N)` ≠ `None`) and
//! never sends a transaction.
//!
//! Write subcommands (`register-node`, `register-file`) refuse to
//! execute against a non-local RPC URL unless the operator passes
//! `--allow-live-chain-write`. "Local" is `localhost`, `127.0.0.1`,
//! or `::1`. This protects live chains without breaking
//! local-mirror automation.

use std::process;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;

// Import from the sum-node library crate.
use sum_net::identity;
use sum_node::rpc_client::L1RpcClient;
use sum_node::tx_builder;

#[derive(Parser)]
#[command(
    name = "e2e-helper",
    about = "E2E test helper for SUM Chain L1 interactions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check if the L1 RPC is reachable.
    Health {
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
    },

    /// Print the L1 base58 address for a given seed.
    L1Address {
        #[arg(long)]
        seed_hex: String,
    },

    /// Query account balance.
    Balance {
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
        #[arg(long)]
        address: String,
    },

    /// Query current block number.
    BlockNumber {
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
    },

    /// Query node registry record.
    NodeRecord {
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
        #[arg(long)]
        address: String,
    },

    /// Query active PoR challenges for a node.
    ActiveChallenges {
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
        #[arg(long)]
        address: String,
    },

    /// Register as an ArchiveNode on the L1.
    RegisterNode {
        #[arg(long)]
        seed_hex: String,
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
        #[arg(long, default_value = "1000000000")]
        stake: u64,
        /// Authorize this write against a non-local RPC URL. Without
        /// this flag, the command refuses to run unless the RPC URL
        /// resolves to localhost / 127.0.0.1 / ::1. See `docs/CHAIN-COMPAT.md`.
        #[arg(long)]
        allow_live_chain_write: bool,
    },

    /// Register a file's metadata on the L1.
    RegisterFile {
        #[arg(long)]
        seed_hex: String,
        #[arg(long, default_value = "http://127.0.0.1:8545")]
        rpc_url: String,
        /// Merkle root as hex (no 0x prefix).
        #[arg(long)]
        merkle_root: String,
        #[arg(long)]
        total_size: u64,
        #[arg(long, default_value = "100000000")]
        fee_deposit: u64,
        /// Authorize this write against a non-local RPC URL. Without
        /// this flag, the command refuses to run unless the RPC URL
        /// resolves to localhost / 127.0.0.1 / ::1. See `docs/CHAIN-COMPAT.md`.
        #[arg(long)]
        allow_live_chain_write: bool,
    },

    /// Read-only chain smoke probe. Reports RPC reachability, V2
    /// enablement state per `Option<u64> v2_enabled_from_height`, and
    /// optionally exercises additional read paths if known
    /// addresses / roots / tx hashes are supplied. Never sends a
    /// transaction.
    Smoke {
        #[arg(long)]
        rpc_url: String,
        /// Opt-in: also exercise `account_getEncryptionPublicKey`. CLI
        /// flag overrides `SNIP_SMOKE_KNOWN_ADDRESS` env if both set.
        #[arg(long, env = "SNIP_SMOKE_KNOWN_ADDRESS")]
        known_address: Option<String>,
        /// Opt-in: also exercise `storage_getFileInfoV2`. CLI flag
        /// overrides `SNIP_SMOKE_KNOWN_ROOT` env if both set.
        #[arg(long, env = "SNIP_SMOKE_KNOWN_ROOT")]
        known_root: Option<String>,
        /// Opt-in: also exercise `chain_getTransactionStatus`. CLI flag
        /// overrides `SNIP_SMOKE_KNOWN_TX` env if both set.
        #[arg(long, env = "SNIP_SMOKE_KNOWN_TX")]
        known_tx: Option<String>,
        /// Emit a single JSON object instead of human-readable lines.
        #[arg(long)]
        json: bool,
        /// Treat `DISABLED` and `PENDING` V2 states as failure (exit 1).
        /// Useful for environments where V2 is expected to be live.
        /// No env equivalent — flag-only on purpose so each invocation
        /// states intent explicitly.
        #[arg(long)]
        require_v2: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(exit_code) => process::exit(exit_code),
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            process::exit(1);
        }
    }
}

/// `run` returns the desired process exit code. `0` is success;
/// `1` is a runtime / wire / chain failure (or `--require-v2`
/// unmet); `2` is operator misuse (e.g. write subcommand against
/// non-local RPC URL without `--allow-live-chain-write`).
async fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Health { rpc_url } => {
            let rpc = L1RpcClient::new(rpc_url.clone());
            let result: serde_json::Value =
                rpc.call_public("health", serde_json::json!([])).await?;
            println!("OK: L1 reachable at {rpc_url}");
            println!("{}", serde_json::to_string_pretty(&result)?);
        }

        Command::L1Address { seed_hex } => {
            let seed = parse_seed(&seed_hex)?;
            let kp = identity::keypair_from_seed(&seed)?;
            let addr = identity::l1_address_from_keypair(&kp);
            println!("{}", identity::l1_address_base58(&addr));
        }

        Command::Balance { rpc_url, address } => {
            let rpc = L1RpcClient::new(rpc_url);
            let balance: serde_json::Value = rpc
                .call_public("get_balance", serde_json::json!([address]))
                .await?;
            println!("{balance}");
        }

        Command::BlockNumber { rpc_url } => {
            let rpc = L1RpcClient::new(rpc_url);
            let height: serde_json::Value = rpc
                .call_public("sum_blockNumber", serde_json::json!([]))
                .await?;
            println!("{height}");
        }

        Command::NodeRecord { rpc_url, address } => {
            let rpc = L1RpcClient::new(rpc_url);
            let record = rpc.get_node_record(&address).await?;
            match record {
                Some(r) => println!("{}", serde_json::to_string_pretty(&r)?),
                None => println!("null"),
            }
        }

        Command::ActiveChallenges { rpc_url, address } => {
            let rpc = L1RpcClient::new(rpc_url);
            let challenges = rpc.get_active_challenges(&address).await?;
            println!("{}", serde_json::to_string_pretty(&challenges)?);
        }

        Command::RegisterNode {
            seed_hex,
            rpc_url,
            stake,
            allow_live_chain_write,
        } => {
            if let Some(code) = check_write_gate(&rpc_url, allow_live_chain_write) {
                return Ok(code);
            }
            let seed = parse_seed(&seed_hex)?;
            let rpc = L1RpcClient::new(rpc_url);

            let kp = identity::keypair_from_seed(&seed)?;
            let addr = identity::l1_address_from_keypair(&kp);
            let addr_b58 = identity::l1_address_base58(&addr);

            let nonce = rpc.get_nonce(&addr_b58).await?;
            let chain_id = rpc.get_chain_id().await?;

            let tx_hex = tx_builder::build_register_archive_node_tx(
                &seed, chain_id, nonce, 1_000_000, stake,
            )?;

            let result = rpc.send_raw_transaction(&tx_hex).await?;
            println!("Submitted RegisterNode tx: {result}");
        }

        Command::RegisterFile {
            seed_hex,
            rpc_url,
            merkle_root,
            total_size,
            fee_deposit,
            allow_live_chain_write,
        } => {
            if let Some(code) = check_write_gate(&rpc_url, allow_live_chain_write) {
                return Ok(code);
            }
            let seed = parse_seed(&seed_hex)?;
            let rpc = L1RpcClient::new(rpc_url);

            let kp = identity::keypair_from_seed(&seed)?;
            let addr = identity::l1_address_from_keypair(&kp);
            let addr_b58 = identity::l1_address_base58(&addr);

            let nonce = rpc.get_nonce(&addr_b58).await?;
            let chain_id = rpc.get_chain_id().await?;

            let root_bytes = hex::decode(&merkle_root)?;
            if root_bytes.len() != 32 {
                anyhow::bail!("merkle_root must be 64 hex chars (32 bytes)");
            }
            let mut root = [0u8; 32];
            root.copy_from_slice(&root_bytes);

            let tx_hex = tx_builder::build_register_file_tx(
                &seed,
                chain_id,
                nonce,
                1_000_000,
                root,
                total_size,
                vec![],
                fee_deposit,
            )?;

            let result = rpc.send_raw_transaction(&tx_hex).await?;
            println!("Submitted RegisterFile tx: {result}");
        }

        Command::Smoke {
            rpc_url,
            known_address,
            known_root,
            known_tx,
            json,
            require_v2,
        } => {
            let rpc = L1RpcClient::new(rpc_url.clone());
            let opts = SmokeOpts {
                known_address,
                known_root,
                known_tx,
                require_v2,
            };
            let report = build_smoke_report(&rpc, &rpc_url, &opts).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", format_smoke_human(&report));
            }
            return Ok(if report.ok { 0 } else { 1 });
        }
    }
    Ok(0)
}

// ── Smoke types + helpers ────────────────────────────────────────────────────

struct SmokeOpts {
    known_address: Option<String>,
    known_root: Option<String>,
    known_tx: Option<String>,
    require_v2: bool,
}

#[derive(Debug, Serialize)]
struct SmokeReport {
    ok: bool,
    rpc_url: String,
    checks: Vec<CheckResult>,
    v2_state: Option<V2StateReport>,
    skipped: Vec<SkippedCheck>,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: String,
    status: &'static str, // "ok" | "fail"
    detail: String,
}

#[derive(Debug, Serialize)]
struct SkippedCheck {
    name: String,
    reason: String,
}

/// Wire-shape report of the V2 enablement state. The
/// `v2_enabled_from_height` field is a load-bearing `Option<u64>`:
/// `Some(0)` (V2 enabled from genesis), `Some(N)` (V2 enabled at
/// height N), `None` (V2 disabled). serde serializes the three cases
/// as JSON `0`, `N`, and `null` — distinguishable to any consumer.
#[derive(Debug, Serialize)]
struct V2StateReport {
    state: &'static str, // "ENABLED_FROM_GENESIS" | "ENABLED_FROM_HEIGHT" | "PENDING" | "DISABLED"
    v2_enabled_from_height: Option<u64>,
    finalized_height: u64,
    activation_height: Option<u64>,
    blocks_remaining: Option<u64>,
}

/// Pure V2 state classifier. Same predicate as `access.rs`,
/// `main.rs`, and `ingest_v2.rs` use to gate V2 transactions; pulled
/// into a function here so the smoke output and the gate logic
/// cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V2State {
    EnabledFromGenesis {
        finalized_height: u64,
    },
    EnabledFromHeight {
        activation_height: u64,
        finalized_height: u64,
    },
    Pending {
        activation_height: u64,
        finalized_height: u64,
        blocks_remaining: u64,
    },
    Disabled,
}

fn classify_v2_state(v2_enabled_from_height: Option<u64>, finalized_height: u64) -> V2State {
    match v2_enabled_from_height {
        Some(0) => V2State::EnabledFromGenesis { finalized_height },
        Some(h) if finalized_height >= h => V2State::EnabledFromHeight {
            activation_height: h,
            finalized_height,
        },
        Some(h) => V2State::Pending {
            activation_height: h,
            finalized_height,
            blocks_remaining: h - finalized_height,
        },
        None => V2State::Disabled,
    }
}

impl V2State {
    fn is_enabled(&self) -> bool {
        matches!(
            self,
            V2State::EnabledFromGenesis { .. } | V2State::EnabledFromHeight { .. }
        )
    }

    fn into_report(self, v2_enabled_from_height: Option<u64>) -> V2StateReport {
        match self {
            V2State::EnabledFromGenesis { finalized_height } => V2StateReport {
                state: "ENABLED_FROM_GENESIS",
                v2_enabled_from_height,
                finalized_height,
                activation_height: None,
                blocks_remaining: None,
            },
            V2State::EnabledFromHeight {
                activation_height,
                finalized_height,
            } => V2StateReport {
                state: "ENABLED_FROM_HEIGHT",
                v2_enabled_from_height,
                finalized_height,
                activation_height: Some(activation_height),
                blocks_remaining: None,
            },
            V2State::Pending {
                activation_height,
                finalized_height,
                blocks_remaining,
            } => V2StateReport {
                state: "PENDING",
                v2_enabled_from_height,
                finalized_height,
                activation_height: Some(activation_height),
                blocks_remaining: Some(blocks_remaining),
            },
            V2State::Disabled => V2StateReport {
                state: "DISABLED",
                v2_enabled_from_height,
                finalized_height: 0,
                activation_height: None,
                blocks_remaining: None,
            },
        }
    }
}

impl std::fmt::Display for V2State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            V2State::EnabledFromGenesis { finalized_height } => write!(
                f,
                "ENABLED (from genesis; finalized height {finalized_height})"
            ),
            V2State::EnabledFromHeight {
                activation_height,
                finalized_height,
            } => write!(
                f,
                "ENABLED (from height {activation_height}; finalized height {finalized_height} >= {activation_height})"
            ),
            V2State::Pending {
                activation_height,
                finalized_height,
                blocks_remaining,
            } => write!(
                f,
                "PENDING (from height {activation_height}; finalized height {finalized_height} < {activation_height}, {blocks_remaining} blocks remaining)"
            ),
            V2State::Disabled => {
                write!(f, "DISABLED (chain returned v2_enabled_from_height: null)")
            }
        }
    }
}

/// Check whether `url`'s host is one of the loopback names that we
/// treat as "local" for the write-gate. Anything else (DNS host,
/// public IP, internal-network host) requires
/// `--allow-live-chain-write` to authorize the write.
fn is_local_rpc_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host_with_port = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // IPv6 hosts are bracketed: "[::1]:8545" → "::1".
    let host = if let Some(stripped) = host_with_port.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        host_with_port.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Returns `Some(2)` if the operator should be told "no" with exit
/// code 2 (misuse). Returns `None` if the write is authorized.
fn check_write_gate(rpc_url: &str, allow_live_chain_write: bool) -> Option<i32> {
    if is_local_rpc_url(rpc_url) || allow_live_chain_write {
        return None;
    }
    eprintln!("REFUSED: write subcommand against non-local RPC URL ({rpc_url}).");
    eprintln!("         Local URLs are localhost / 127.0.0.1 / ::1.");
    eprintln!("         Pass --allow-live-chain-write to authorize this write.");
    Some(2)
}

async fn build_smoke_report(rpc: &L1RpcClient, rpc_url: &str, opts: &SmokeOpts) -> SmokeReport {
    let mut checks = Vec::new();
    let mut skipped = Vec::new();

    // Check 1: chain_getChainParams (also gives us v2_enabled_from_height).
    let params = match rpc.chain_get_chain_params().await {
        Ok(p) => {
            checks.push(CheckResult {
                name: "chain_getChainParams".into(),
                status: "ok",
                detail: format!(
                    "chain_id={}, R={}, v2_enabled_from_height={:?}",
                    p.chain_id, p.assignment_replication_factor, p.v2_enabled_from_height
                ),
            });
            p
        }
        Err(e) => {
            checks.push(CheckResult {
                name: "chain_getChainParams".into(),
                status: "fail",
                detail: format!("{e:#}"),
            });
            return SmokeReport {
                ok: false,
                rpc_url: rpc_url.to_string(),
                checks,
                v2_state: None,
                skipped,
            };
        }
    };

    // Check 2: chain_getBlockHeight (must echo back finality="finalized").
    //
    // We deliberately validate the echo'd `finality` string, NOT the
    // height. A fresh local mirror is legitimately at height 0 with
    // `finality = "finalized"`; rejecting height==0 would break it.
    // The echo'd string IS load-bearing: the chain returns it
    // verbatim from the request param so a caller that forgot to
    // pass `["finalized"]` (or hit a drifted RPC shape) gets caught
    // here instead of silently treating "latest" as final.
    let head = match rpc.chain_get_block_height().await {
        Ok(h) if h.finality == "finalized" => {
            checks.push(CheckResult {
                name: "chain_getBlockHeight".into(),
                status: "ok",
                detail: format!("finalized height={}, finality={}", h.height, h.finality),
            });
            h
        }
        Ok(h) => {
            checks.push(CheckResult {
                name: "chain_getBlockHeight".into(),
                status: "fail",
                detail: format!(
                    "expected finality=\"finalized\", got finality={:?} \
                     (height={}); chain may have ignored the [\"finalized\"] \
                     param or the RPC wire shape has drifted",
                    h.finality, h.height
                ),
            });
            return SmokeReport {
                ok: false,
                rpc_url: rpc_url.to_string(),
                checks,
                v2_state: None,
                skipped,
            };
        }
        Err(e) => {
            checks.push(CheckResult {
                name: "chain_getBlockHeight".into(),
                status: "fail",
                detail: format!("{e:#}"),
            });
            return SmokeReport {
                ok: false,
                rpc_url: rpc_url.to_string(),
                checks,
                v2_state: None,
                skipped,
            };
        }
    };

    let v2_state = classify_v2_state(params.v2_enabled_from_height, head.height);
    let v2_state_report = v2_state.into_report(params.v2_enabled_from_height);

    // Opt-in: account_getEncryptionPublicKey.
    match opts.known_address.as_deref() {
        Some(addr) => match rpc.account_get_encryption_public_key(addr).await {
            Ok(Some(_)) => checks.push(CheckResult {
                name: "account_getEncryptionPublicKey".into(),
                status: "ok",
                detail: format!("{addr}: registered (32 bytes)"),
            }),
            Ok(None) => checks.push(CheckResult {
                name: "account_getEncryptionPublicKey".into(),
                status: "ok",
                detail: format!("{addr}: not registered (null)"),
            }),
            Err(e) => checks.push(CheckResult {
                name: "account_getEncryptionPublicKey".into(),
                status: "fail",
                detail: format!("{e:#}"),
            }),
        },
        None => skipped.push(SkippedCheck {
            name: "account_getEncryptionPublicKey".into(),
            reason: "no --known-address provided".into(),
        }),
    }

    // Opt-in: storage_getFileInfoV2.
    match opts.known_root.as_deref() {
        Some(root) => match rpc.storage_get_file_info_v2(root, None, None).await {
            Ok(info) => checks.push(CheckResult {
                name: "storage_getFileInfoV2".into(),
                status: "ok",
                detail: format!(
                    "lifecycle={:?}, visibility={:?}, chunk_count={}",
                    info.lifecycle, info.visibility, info.chunk_count
                ),
            }),
            Err(e) => checks.push(CheckResult {
                name: "storage_getFileInfoV2".into(),
                status: "fail",
                detail: format!("{e:#}"),
            }),
        },
        None => skipped.push(SkippedCheck {
            name: "storage_getFileInfoV2".into(),
            reason: "no --known-root provided".into(),
        }),
    }

    // Opt-in: chain_getTransactionStatus.
    match opts.known_tx.as_deref() {
        Some(tx) => match rpc.chain_get_transaction_status(tx).await {
            Ok(s) => checks.push(CheckResult {
                name: "chain_getTransactionStatus".into(),
                status: "ok",
                detail: format!("{s:?}"),
            }),
            Err(e) => checks.push(CheckResult {
                name: "chain_getTransactionStatus".into(),
                status: "fail",
                detail: format!("{e:#}"),
            }),
        },
        None => skipped.push(SkippedCheck {
            name: "chain_getTransactionStatus".into(),
            reason: "no --known-tx provided".into(),
        }),
    }

    let any_failed = checks.iter().any(|c| c.status == "fail");
    let v2_block = opts.require_v2 && !v2_state.is_enabled();
    let ok = !any_failed && !v2_block;

    SmokeReport {
        ok,
        rpc_url: rpc_url.to_string(),
        checks,
        v2_state: Some(v2_state_report),
        skipped,
    }
}

fn format_smoke_human(report: &SmokeReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "smoke target: {}", report.rpc_url);
    for (i, c) in report.checks.iter().enumerate() {
        let _ = writeln!(
            out,
            "[{}/{}] {:.<32} {} ({})",
            i + 1,
            report.checks.len(),
            format!("{} ", c.name),
            c.status.to_uppercase(),
            c.detail
        );
    }
    if let Some(v) = &report.v2_state {
        let _ = writeln!(
            out,
            "V2 state: {} (v2_enabled_from_height={})",
            v.state,
            match v.v2_enabled_from_height {
                Some(0) => "Some(0) → from genesis".to_string(),
                Some(n) => format!("Some({n}) → enabled at height {n}"),
                None => "None → V2 disabled".to_string(),
            }
        );
    }
    if !report.skipped.is_empty() {
        let _ = writeln!(out);
        for s in &report.skipped {
            let _ = writeln!(out, "(skipped: {} — {})", s.name, s.reason);
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "smoke: {}", if report.ok { "ok" } else { "FAILED" });
    out
}

fn parse_seed(hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex)?;
    if bytes.len() != 32 {
        anyhow::bail!("seed must be 64 hex chars (32 bytes), got {}", bytes.len());
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Ok(seed)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    // ── classify_v2_state ───────────────────────────────────────────

    /// `Some(0)` is the load-bearing distinction: V2 enabled from
    /// genesis, NOT the same as `None` (V2 disabled).
    #[test]
    fn classify_v2_state_some_zero_is_enabled_from_genesis() {
        let s = classify_v2_state(Some(0), 100);
        assert_eq!(
            s,
            V2State::EnabledFromGenesis {
                finalized_height: 100
            }
        );
        assert!(s.is_enabled());
    }

    /// `Some(N)` with finalized < N is `Pending`, with `blocks_remaining
    /// = N - finalized`.
    #[test]
    fn classify_v2_state_some_n_below_head_is_pending() {
        let s = classify_v2_state(Some(500), 100);
        assert_eq!(
            s,
            V2State::Pending {
                activation_height: 500,
                finalized_height: 100,
                blocks_remaining: 400,
            }
        );
        assert!(!s.is_enabled());
    }

    /// `Some(N)` with finalized >= N is `EnabledFromHeight`.
    #[test]
    fn classify_v2_state_some_n_at_or_above_head_is_enabled_from_height() {
        let s = classify_v2_state(Some(500), 500);
        assert_eq!(
            s,
            V2State::EnabledFromHeight {
                activation_height: 500,
                finalized_height: 500,
            }
        );
        assert!(s.is_enabled());

        let s2 = classify_v2_state(Some(500), 501);
        assert!(matches!(s2, V2State::EnabledFromHeight { .. }));
        assert!(s2.is_enabled());
    }

    /// `None` is always `Disabled` regardless of finalized height.
    #[test]
    fn classify_v2_state_none_is_disabled() {
        let s = classify_v2_state(None, 100);
        assert_eq!(s, V2State::Disabled);
        assert!(!s.is_enabled());

        let s2 = classify_v2_state(None, 0);
        assert_eq!(s2, V2State::Disabled);
    }

    /// `--require-v2` rejects only `Disabled` and `Pending`.
    /// `EnabledFromGenesis` and `EnabledFromHeight` pass.
    #[test]
    fn require_v2_passes_only_for_enabled_states() {
        assert!(classify_v2_state(Some(0), 0).is_enabled());
        assert!(classify_v2_state(Some(100), 100).is_enabled());
        assert!(!classify_v2_state(Some(100), 99).is_enabled());
        assert!(!classify_v2_state(None, 100).is_enabled());
    }

    /// V2StateReport carries the original `Option<u64>` so the JSON
    /// output preserves the `Some(0) != None` distinction.
    #[test]
    fn v2_state_report_preserves_some_zero() {
        let r = classify_v2_state(Some(0), 100).into_report(Some(0));
        assert_eq!(r.state, "ENABLED_FROM_GENESIS");
        assert_eq!(r.v2_enabled_from_height, Some(0));
        assert!(r.activation_height.is_none());

        let r2 = classify_v2_state(None, 100).into_report(None);
        assert_eq!(r2.state, "DISABLED");
        assert_eq!(r2.v2_enabled_from_height, None);

        // JSON serialization preserves the distinction — Some(0) → 0,
        // None → null. This is the test that fails first if a future
        // serde-default refactor flattens them.
        let json_some_zero = serde_json::to_string(&r).unwrap();
        assert!(json_some_zero.contains("\"v2_enabled_from_height\":0"));
        let json_none = serde_json::to_string(&r2).unwrap();
        assert!(json_none.contains("\"v2_enabled_from_height\":null"));
    }

    // ── is_local_rpc_url ───────────────────────────────────────────

    #[test]
    fn is_local_rpc_url_localhost_variants() {
        assert!(is_local_rpc_url("http://localhost"));
        assert!(is_local_rpc_url("http://localhost:8545"));
        assert!(is_local_rpc_url("https://localhost:8545"));
        assert!(is_local_rpc_url("http://localhost:8545/"));
        assert!(is_local_rpc_url("http://localhost:8545/path?q=1"));
    }

    #[test]
    fn is_local_rpc_url_127() {
        assert!(is_local_rpc_url("http://127.0.0.1:8545"));
        assert!(is_local_rpc_url("http://127.0.0.1"));
        assert!(is_local_rpc_url("http://127.0.0.1:9944/v1"));
    }

    #[test]
    fn is_local_rpc_url_ipv6() {
        assert!(is_local_rpc_url("http://[::1]:8545"));
        assert!(is_local_rpc_url("http://[::1]"));
        assert!(is_local_rpc_url("http://[::1]:9944/path"));
    }

    #[test]
    fn is_local_rpc_url_rejects_non_local() {
        assert!(!is_local_rpc_url("http://example.com:8545"));
        assert!(!is_local_rpc_url("https://chain.live.example/v1"));
        assert!(!is_local_rpc_url("http://10.0.0.1:8545"));
        assert!(!is_local_rpc_url("http://192.168.1.1:8545"));
        // 0.0.0.0 is "any-bind"; we don't treat it as a connect target.
        assert!(!is_local_rpc_url("http://0.0.0.0:8545"));
        // Hostname starting with "localhost" but with an extra suffix is NOT local.
        assert!(!is_local_rpc_url("http://localhost.attacker.example:8545"));
    }

    #[test]
    fn is_local_rpc_url_handles_no_scheme() {
        // Defensive: if a caller forgot the scheme, host parse still works.
        assert!(is_local_rpc_url("localhost:8545"));
        assert!(is_local_rpc_url("127.0.0.1:8545"));
        assert!(!is_local_rpc_url("example.com:8545"));
    }

    // ── check_write_gate ───────────────────────────────────────────

    #[test]
    fn check_write_gate_local_passes_without_flag() {
        assert!(check_write_gate("http://localhost:8545", false).is_none());
        assert!(check_write_gate("http://127.0.0.1:8545", false).is_none());
        assert!(check_write_gate("http://[::1]:8545", false).is_none());
    }

    #[test]
    fn check_write_gate_non_local_blocks_without_flag_and_passes_with_flag() {
        assert_eq!(
            check_write_gate("https://chain.live.example", false),
            Some(2),
            "non-local URL without flag must refuse with exit 2"
        );
        assert!(
            check_write_gate("https://chain.live.example", true).is_none(),
            "non-local URL with flag is authorized"
        );
    }

    // ── Mocked HTTP smoke (real wire-shape regression guard) ───────

    fn make_chain_params_json(v2_enabled_from_height: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "chain_id": 31337,
            "block_time_ms": 2000,
            "max_block_bytes": 8388608,
            "max_txs_per_block": 1024,
            "min_fee": 1000,
            "finality_depth": 3,
            "storage_fee_per_byte": 1,
            "max_metadata_bytes": 65536,
            "max_access_list_bytes": 16384,
            "activation_grace_blocks": 50,
            "abandonment_fee_percent": 10,
            "max_chunk_count_per_file": 1048576,
            "max_chunk_indices_per_tx": 65536,
            "assignment_replication_factor": 3,
            "v2_enabled_from_height": v2_enabled_from_height,
        })
    }

    fn jsonrpc_response(id: i64, result: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    /// End-to-end smoke against a local mock HTTP server: the actual
    /// wire path that production smoke runs. Validates that the
    /// `chain_getChainParams` + `chain_getBlockHeight` decoders + the
    /// V2 classifier produce a `SmokeReport { ok: true }` against a
    /// canonical V2-enabled-from-genesis chain.
    #[tokio::test]
    async fn smoke_happy_path_against_mocked_chain() {
        let server = MockServer::start_async().await;

        // The real `L1RpcClient::call` posts a JSON-RPC envelope and
        // expects `{ jsonrpc, id, result }` back. The mock dispatches
        // by method name in the request body.
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getChainParams");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        1,
                        make_chain_params_json(serde_json::json!(0)),
                    ));
            })
            .await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getBlockHeight");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        2,
                        serde_json::json!({"height": 12345, "finality": "finalized"}),
                    ));
            })
            .await;

        let rpc_url = server.base_url();
        let rpc = L1RpcClient::new(rpc_url.clone());
        let opts = SmokeOpts {
            known_address: None,
            known_root: None,
            known_tx: None,
            require_v2: false,
        };
        let report = build_smoke_report(&rpc, &rpc_url, &opts).await;

        assert!(report.ok, "happy-path smoke should pass: {:#?}", report);
        let v2 = report.v2_state.expect("v2 state should be reported");
        assert_eq!(v2.state, "ENABLED_FROM_GENESIS");
        assert_eq!(
            v2.v2_enabled_from_height,
            Some(0),
            "Some(0) must NOT be flattened to None"
        );
        assert_eq!(v2.finalized_height, 12345);
    }

    /// Fresh local mirror: chain at finalized height 0 with
    /// `v2_enabled_from_height=0` is a legitimate "V2 enabled from
    /// genesis, no blocks yet" state. Smoke MUST pass — height 0 is
    /// not an error, only the `finality` echo string is gated.
    #[tokio::test]
    async fn smoke_height_zero_with_finalized_passes() {
        let server = MockServer::start_async().await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getChainParams");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        1,
                        make_chain_params_json(serde_json::json!(0)),
                    ));
            })
            .await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getBlockHeight");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        2,
                        serde_json::json!({"height": 0, "finality": "finalized"}),
                    ));
            })
            .await;

        let rpc_url = server.base_url();
        let rpc = L1RpcClient::new(rpc_url.clone());
        let opts = SmokeOpts {
            known_address: None,
            known_root: None,
            known_tx: None,
            require_v2: false,
        };
        let report = build_smoke_report(&rpc, &rpc_url, &opts).await;

        assert!(
            report.ok,
            "fresh-mirror (height 0, finality=finalized, v2=Some(0)) must pass smoke: {:#?}",
            report
        );
    }

    /// Finality echo gate: chain returns `finality: "latest"` (caller
    /// forgot the `["finalized"]` param, or RPC drift). Smoke MUST
    /// fail loudly on the chain_getBlockHeight check rather than
    /// silently treating an un-finalized height as final.
    #[tokio::test]
    async fn smoke_finality_not_finalized_fails() {
        let server = MockServer::start_async().await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getChainParams");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        1,
                        make_chain_params_json(serde_json::json!(0)),
                    ));
            })
            .await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getBlockHeight");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        2,
                        serde_json::json!({"height": 100, "finality": "latest"}),
                    ));
            })
            .await;

        let rpc_url = server.base_url();
        let rpc = L1RpcClient::new(rpc_url.clone());
        let opts = SmokeOpts {
            known_address: None,
            known_root: None,
            known_tx: None,
            require_v2: false,
        };
        let report = build_smoke_report(&rpc, &rpc_url, &opts).await;

        assert!(
            !report.ok,
            "finality=\"latest\" must fail smoke: {:#?}",
            report
        );
        let bh_check = report
            .checks
            .iter()
            .find(|c| c.name == "chain_getBlockHeight")
            .expect("chain_getBlockHeight should appear in checks");
        assert_eq!(bh_check.status, "fail");
        assert!(
            bh_check.detail.contains("expected finality=\"finalized\""),
            "fail detail should explain the contract: {:?}",
            bh_check.detail
        );
    }

    /// Wire-shape drift regression: chain emits a non-numeric
    /// `v2_enabled_from_height` (e.g. accidentally a string in a
    /// future version, or chain returns an array). Smoke must
    /// surface the decode failure as a check fail, not silently
    /// fall through with a default.
    #[tokio::test]
    async fn smoke_malformed_v2_enabled_from_height_fails_decode() {
        let server = MockServer::start_async().await;

        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_contains("chain_getChainParams");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(jsonrpc_response(
                        1,
                        make_chain_params_json(serde_json::json!("oops_a_string")),
                    ));
            })
            .await;

        let rpc_url = server.base_url();
        let rpc = L1RpcClient::new(rpc_url.clone());
        let opts = SmokeOpts {
            known_address: None,
            known_root: None,
            known_tx: None,
            require_v2: false,
        };
        let report = build_smoke_report(&rpc, &rpc_url, &opts).await;

        assert!(
            !report.ok,
            "malformed v2_enabled_from_height must fail smoke: {:#?}",
            report
        );
        let chain_params_check = report
            .checks
            .iter()
            .find(|c| c.name == "chain_getChainParams")
            .expect("chain_getChainParams should appear in checks");
        assert_eq!(chain_params_check.status, "fail");
    }
}
