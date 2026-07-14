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

use std::collections::BTreeMap;
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

    /// Read-only chain snapshot of active node records at a given
    /// height. Operator pre-flight gate before first ingest, scriptable
    /// via `--require-archives N` (exit 2 when below threshold).
    ///
    /// Counts ArchiveNode/Active rows from
    /// `storage_getActiveNodesAtHeight`. Other roles + statuses are
    /// reported in the breakdown but don't gate the exit code.
    /// Mirrors the chain plan's `assignment_replication_factor = 3`
    /// pre-flight: ingest cannot satisfy quorum below R distinct
    /// archive identities.
    ActiveNodesAtHeight {
        #[arg(long)]
        rpc_url: String,
        /// `finalized` (default) or an explicit u64. `finalized` calls
        /// `chain_getBlockHeight(["finalized"])` first, then queries
        /// the active-nodes snapshot at that height.
        #[arg(long, default_value = "finalized")]
        height: String,
        /// If set, require at least N ArchiveNode/Active rows; exit 2
        /// if the count is below the threshold. No env equivalent —
        /// flag-only on purpose so each invocation states intent
        /// explicitly (matches the `--require-v2` precedent on
        /// `smoke`).
        #[arg(long)]
        require_archives: Option<usize>,
        /// Emit a single JSON object instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },

    /// Generate fresh Ed25519 seeds for the WS2b local-mirror E2E
    /// suite and print an `extra-alloc` JSON snippet the operator
    /// pastes into the chain mirror's funding overlay.
    ///
    /// Seeds are written to `<out>/<role>.seed.hex` (one file per
    /// role). The output directory MUST be empty or non-existent —
    /// the command refuses to overwrite existing seeds. Files are
    /// `0o600`. Generated seeds are local-only; pair with a
    /// `.gitignore` entry covering the output dir.
    ///
    /// Default roles: `owner`, `recipient`, `third_party`,
    /// `archive_1`, `archive_2`, `archive_3`. The chain plan
    /// fixes `assignment_replication_factor = 3`, so the
    /// WS2b harness needs three archive identities to satisfy
    /// quorum during ingest scenarios. The harness in WS2b Commit 2
    /// references these names.
    GenerateE2eKeys {
        /// Output directory for the generated seed files.
        #[arg(long)]
        out: std::path::PathBuf,
        /// Default balance (in chain base units) to suggest for each
        /// generated address in the printed extra-alloc snippet.
        /// Operators edit before pasting if a different value is
        /// needed; this is a hint, not a chain-side authority.
        ///
        /// Parsed as `u128` and emitted as a JSON **number** (not a
        /// string) — the chain mirror's `extra-alloc.json` schema
        /// requires `{ "<base58>": <integer>, ... }` with numeric
        /// balances.
        #[arg(long, default_value = "1000000000000")]
        balance: u128,
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

        Command::ActiveNodesAtHeight {
            rpc_url,
            height,
            require_archives,
            json,
        } => {
            let rpc = L1RpcClient::new(rpc_url.clone());
            let report = build_active_nodes_report(&rpc, &height).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", format_active_nodes_human(&report));
            }
            return Ok(active_nodes_exit_code(&report, require_archives));
        }

        Command::GenerateE2eKeys { out, balance } => {
            // Writes one seed file per WS2b role and prints a JSON
            // snippet of public addresses + balances for the
            // operator's `extra-alloc.json` overlay. Refuses to
            // overwrite existing keys.
            let snippet = generate_e2e_keys(&out, balance)?;
            println!("{snippet}");
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
            Ok(Some(info)) => checks.push(CheckResult {
                name: "storage_getFileInfoV2".into(),
                status: "ok",
                detail: format!(
                    "lifecycle={:?}, visibility={:?}, chunk_count={}",
                    info.lifecycle, info.visibility, info.chunk_count
                ),
            }),
            // Null result: the endpoint responded correctly, the file just
            // isn't registered. The RPC contract is satisfied.
            Ok(None) => checks.push(CheckResult {
                name: "storage_getFileInfoV2".into(),
                status: "ok",
                detail: "no V2 row (null result — file not registered)".into(),
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

// ── Active-nodes preflight ───────────────────────────────────────────────────

/// Snapshot returned by `ActiveNodesAtHeight`. Captures both the
/// height we resolved against (so a future `finalized` query is
/// reproducible) and the role/status breakdown the operator wants
/// to gate on.
#[derive(Debug, Serialize)]
struct ActiveNodesReport {
    /// Effective height the snapshot was taken at. When the caller
    /// passes `--height finalized`, this is the value `chain_getBlockHeight`
    /// returned at probe time; with an explicit u64, it's the input.
    height: u64,
    /// Number of `ArchiveNode` rows with `status == "Active"`. This
    /// is what `--require-archives` gates on.
    active_archives: usize,
    /// Full role × status breakdown so operators can spot stuck
    /// `Pending`/`Slashed` rows that don't satisfy quorum.
    by_role_status: BTreeMap<String, BTreeMap<String, usize>>,
}

/// Resolve `--height` (either `finalized` or an explicit u64), call
/// `storage_getActiveNodesAtHeight`, and tally the breakdown.
async fn build_active_nodes_report(
    rpc: &L1RpcClient,
    height_arg: &str,
) -> Result<ActiveNodesReport> {
    let effective_height = if height_arg.eq_ignore_ascii_case("finalized") {
        rpc.chain_get_block_height()
            .await
            .map_err(|e| anyhow::anyhow!("chain_getBlockHeight: {e}"))?
            .height
    } else {
        height_arg
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("--height must be `finalized` or a u64: {e}"))?
    };

    let nodes = rpc
        .storage_get_active_nodes_at_height(effective_height)
        .await
        .map_err(|e| anyhow::anyhow!("storage_getActiveNodesAtHeight({effective_height}): {e}"))?;

    let by_role_status = tally_active_nodes(&nodes);
    // Count eligible archives via the shared `is_active_archive`
    // contract. The tally is preserved above for operator visibility
    // (Unbonding, Withdrawn, and unknown-future buckets remain
    // observable), but the `--require-archives` gate keys off the
    // exact contract, not a two-string lookup that would silently
    // pass if either bucket key ever changed.
    let active_archives = nodes.iter().filter(|n| n.is_active_archive()).count();

    Ok(ActiveNodesReport {
        height: effective_height,
        active_archives,
        by_role_status,
    })
}

/// Pure tally — pulled out of `build_active_nodes_report` so unit
/// tests can pin the role × status arithmetic without spinning up
/// a real RPC.
fn tally_active_nodes(
    nodes: &[sum_types::rpc_types::NodeRecordInfo],
) -> BTreeMap<String, BTreeMap<String, usize>> {
    let mut out: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for n in nodes {
        *out.entry(n.role.clone())
            .or_default()
            .entry(n.status.clone())
            .or_insert(0) += 1;
    }
    out
}

/// Render the report as plain operator-readable lines. Mirrors the
/// `smoke`-style key:value layout so operators eyeballing CI logs
/// see consistent shape across both subcommands.
fn format_active_nodes_human(report: &ActiveNodesReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "height: {}", report.height);
    let _ = writeln!(out, "active_archives: {}", report.active_archives);
    if report.by_role_status.is_empty() {
        let _ = writeln!(
            out,
            "(snapshot is empty — chain reports no nodes at this height)"
        );
    } else {
        for (role, statuses) in &report.by_role_status {
            for (status, count) in statuses {
                let _ = writeln!(out, "{role}/{status}: {count}");
            }
        }
    }
    out
}

/// Decide the exit code: `0` when no requirement set or threshold
/// met; `2` when `--require-archives N` is set and the ArchiveNode/
/// Active count is below `N`. RPC/wire failures are bubbled up to
/// the `run` layer's `1` exit via the `?` chain — this function is
/// only invoked when the report is in hand.
fn active_nodes_exit_code(report: &ActiveNodesReport, require_archives: Option<usize>) -> i32 {
    match require_archives {
        Some(threshold) if report.active_archives < threshold => 2,
        _ => 0,
    }
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

// ── generate-e2e-keys ────────────────────────────────────────────────────────

/// Roles the WS2b harness expects. Stable order so a hand-edited
/// overlay stays diff-friendly across regenerations.
///
/// The three `archive_*` roles cover the chain plan's
/// `assignment_replication_factor = 3`: WS2b ingest scenarios spawn
/// all three as concurrent listeners so chain-side quorum is
/// satisfied without lowering R or special-casing chain params.
const E2E_ROLES: &[&str] = &[
    "owner",
    "recipient",
    "third_party",
    "archive_1",
    "archive_2",
    "archive_3",
];

/// Generate fresh Ed25519 seeds, write them to `<out>/<role>.seed.hex`
/// (mode `0o600`), and return a JSON snippet mapping base58
/// addresses to balances. Refuses to overwrite a non-empty `<out>`.
///
/// The snippet matches the chain mirror's `extra-alloc.json` schema
/// exactly — a pure JSON object with **numeric** balances:
///
/// ```json
/// { "<base58-address>": <balance-in-base-units>, ... }
/// ```
///
/// The mirror reads this file at first boot only (fresh-genesis
/// gating). See `docs/OPERATOR-RUNBOOK.md` for the override-file
/// + `down -v` workflow that activates the overlay.
fn generate_e2e_keys(out: &std::path::Path, balance: u128) -> Result<String> {
    use rand_core::{OsRng, RngCore};

    // Refuse to overwrite. `<out>` must not exist OR be empty.
    if out.exists() {
        let mut entries = std::fs::read_dir(out)?;
        if entries.next().is_some() {
            anyhow::bail!(
                "refusing to write into a non-empty directory: {}\n\
                 (existing seed files would be overwritten — pick a fresh path,\n\
                 or wipe the dir manually if you intend to regenerate)",
                out.display()
            );
        }
    } else {
        std::fs::create_dir_all(out)?;
    }

    // Sanity: the dir must be ignored by git so generated seeds
    // don't leak. We don't enforce here — but the operator runbook
    // documents the .gitignore expectation.

    let mut addresses: Vec<String> = Vec::new();
    for role in E2E_ROLES {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let kp = identity::keypair_from_seed(&seed)?;
        let addr = identity::l1_address_from_keypair(&kp);
        let addr_b58 = identity::l1_address_base58(&addr);

        // Hex-encode and write with 0o600 on unix; on non-unix
        // hosts the file is writable but the SNIP project is
        // Linux-only in production so this is acceptable.
        let path = out.join(format!("{role}.seed.hex"));
        let hex_seed = hex::encode(seed);
        std::fs::write(&path, format!("{hex_seed}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        // Hex bytes drop out of scope here (Rust string `Drop`
        // releases without zeroing — fine for ephemeral test seeds
        // that the file already persists, but production code paths
        // should still go through `Zeroizing`).
        let _ = hex_seed;

        addresses.push(addr_b58);
    }

    // Render JSON snippet with stable role order. Balances are
    // emitted as JSON **numbers** (no quotes) per the chain
    // mirror's overlay schema. Public addresses only — never the
    // seed.
    let mut snippet = String::from("{\n");
    for (i, addr) in addresses.iter().enumerate() {
        let comma = if i + 1 == addresses.len() { "" } else { "," };
        snippet.push_str(&format!("  \"{addr}\": {balance}{comma}\n"));
    }
    snippet.push('}');
    Ok(snippet)
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

    // ── generate_e2e_keys ──────────────────────────────────────────

    /// Happy path: empty dir, one seed file per role written, JSON
    /// snippet has the same number of entries with valid base58
    /// addresses + the supplied balance. The role count tracks
    /// `E2E_ROLES.len()` rather than a magic number so adding /
    /// removing a role doesn't silently desync this test.
    #[test]
    fn generate_e2e_keys_writes_seed_files_and_returns_alloc_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let snippet = generate_e2e_keys(dir.path(), 12345).expect("generate");

        // 5 .seed.hex files written.
        for role in E2E_ROLES {
            let path = dir.path().join(format!("{role}.seed.hex"));
            assert!(path.exists(), "{role}.seed.hex must be written");
            let content = std::fs::read_to_string(&path).unwrap();
            // 64 hex chars + trailing newline.
            assert_eq!(
                content.trim_end().len(),
                64,
                "{role}.seed.hex must contain a 64-char hex seed"
            );
            assert!(
                content.trim_end().chars().all(|c| c.is_ascii_hexdigit()),
                "{role}.seed.hex must be hex-only"
            );
            // 0o600 on unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(
                    mode & 0o777,
                    0o600,
                    "{role}.seed.hex must be 0o600, got {:o}",
                    mode & 0o777
                );
            }
        }

        // JSON snippet parses; has 5 entries; every value is a
        // JSON **number** (NOT a string) matching the requested
        // balance; every key is non-empty (a proxy for "valid
        // base58 address" — actual address parsing is
        // `sum_net::identity::l1_address_from_base58`'s job).
        //
        // The JSON-number invariant is load-bearing: the chain
        // mirror's overlay schema is `{ "<base58>": <integer> }`
        // with numeric balances, NOT string-encoded ones. A
        // string-encoded value would fail to parse the overlay at
        // mirror genesis.
        let parsed: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&snippet).expect("JSON snippet must parse");
        assert_eq!(
            parsed.len(),
            E2E_ROLES.len(),
            "alloc entries must match E2E_ROLES count"
        );
        for (addr, bal) in &parsed {
            assert!(!addr.is_empty(), "address must be non-empty");
            assert!(
                bal.is_number(),
                "balance MUST be a JSON number per chain overlay schema, got {bal:?}"
            );
            assert_eq!(
                bal.as_u64(),
                Some(12345),
                "balance must equal the supplied hint as an integer"
            );
        }
    }

    /// **Refuse-to-overwrite invariant.** Generating into a
    /// non-empty dir would silently overwrite existing seed files
    /// and rotate funded test addresses — same risk class as
    /// committing keys. The command refuses; operator must wipe
    /// the dir manually if regeneration is intended.
    #[test]
    fn generate_e2e_keys_refuses_non_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Drop a sentinel file in the dir.
        std::fs::write(dir.path().join("some-existing-seed.hex"), b"DEADBEEF").unwrap();

        let result = generate_e2e_keys(dir.path(), 1);
        let err = result.expect_err("non-empty dir must be refused");
        assert!(
            err.to_string().contains("non-empty directory"),
            "error must explain why: got {err}"
        );

        // Sentinel file is untouched.
        let content = std::fs::read_to_string(dir.path().join("some-existing-seed.hex")).unwrap();
        assert_eq!(content, "DEADBEEF");
    }

    /// Empty dir is accepted. Non-existent dir is also accepted
    /// (we create it). Distinguishing "dir doesn't exist" from
    /// "dir exists and is empty" would force a strange UX with no
    /// real value.
    #[test]
    fn generate_e2e_keys_creates_dir_if_missing() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("does/not/exist/yet");
        assert!(!nested.exists());
        let snippet = generate_e2e_keys(&nested, 1).expect("create + generate");
        assert!(nested.exists());
        assert!(snippet.starts_with('{') && snippet.ends_with('}'));
    }

    /// Each invocation generates fresh seeds — no determinism, no
    /// reuse. Pinning this catches a regression where the OsRng
    /// path got accidentally swapped for a fixed seed (e.g., during
    /// debugging).
    #[test]
    fn generate_e2e_keys_produces_distinct_seeds_across_runs() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let snippet1 = generate_e2e_keys(dir1.path(), 1).unwrap();
        let snippet2 = generate_e2e_keys(dir2.path(), 1).unwrap();
        assert_ne!(
            snippet1, snippet2,
            "two invocations must produce different addresses (OsRng-backed)"
        );
        // Cross-check at the seed-file level too.
        let seed1 = std::fs::read_to_string(dir1.path().join("owner.seed.hex")).unwrap();
        let seed2 = std::fs::read_to_string(dir2.path().join("owner.seed.hex")).unwrap();
        assert_ne!(seed1, seed2, "seeds must differ across runs");
    }

    // ── active_nodes preflight ────────────────────────────────────

    use sum_types::rpc_types::NodeRecordInfo;

    fn n(role: &str, status: &str) -> NodeRecordInfo {
        NodeRecordInfo {
            address: format!("{role}_{status}"),
            role: role.into(),
            staked_balance: 1,
            status: status.into(),
            registered_at: 1,
        }
    }

    /// `tally_active_nodes` is the pure boundary the helper relies
    /// on for the role × status breakdown. Empty input → empty map
    /// (NOT a row of zeros — distinguishes "no nodes" from "no
    /// archives but other roles present").
    #[test]
    fn tally_active_nodes_empty_input_is_empty_map() {
        let tally = tally_active_nodes(&[]);
        assert!(tally.is_empty());
    }

    /// Mixed role/status inventory tallies correctly. Pinning this
    /// catches a regression where the keys got swapped (status
    /// outside / role inside) — the report shape is operator-
    /// facing and changing it would break runbook scripts.
    #[test]
    fn tally_active_nodes_counts_by_role_then_status() {
        let nodes = vec![
            n("ArchiveNode", "Active"),
            n("ArchiveNode", "Active"),
            n("ArchiveNode", "Active"),
            n("ArchiveNode", "Pending"),
            n("Validator", "Active"),
            n("Validator", "Slashed"),
        ];
        let tally = tally_active_nodes(&nodes);
        assert_eq!(tally["ArchiveNode"]["Active"], 3);
        assert_eq!(tally["ArchiveNode"]["Pending"], 1);
        assert_eq!(tally["Validator"]["Active"], 1);
        assert_eq!(tally["Validator"]["Slashed"], 1);
    }

    /// Exit code 0 when no requirement set, regardless of count.
    /// Operators reading a snapshot without gating want the report,
    /// not a failure exit.
    #[test]
    fn active_nodes_exit_code_no_requirement_is_zero_even_when_empty() {
        let report = ActiveNodesReport {
            height: 0,
            active_archives: 0,
            by_role_status: BTreeMap::new(),
        };
        assert_eq!(active_nodes_exit_code(&report, None), 0);
    }

    /// Exit code 0 when requirement met exactly — `>= N` is the
    /// gate, not `> N`. Mainnet bootstraps with exactly 3 archives
    /// in the common case; off-by-one here would block the first
    /// ingest the operator scripted for.
    #[test]
    fn active_nodes_exit_code_meets_threshold_exactly() {
        let report = ActiveNodesReport {
            height: 100,
            active_archives: 3,
            by_role_status: BTreeMap::new(),
        };
        assert_eq!(active_nodes_exit_code(&report, Some(3)), 0);
    }

    /// Exit code 2 when requirement set and count below threshold.
    /// `2` is the documented "operator pre-flight unmet" exit
    /// (matches the convention used elsewhere in this binary for
    /// operator-misuse / pre-flight failures).
    #[test]
    fn active_nodes_exit_code_below_threshold_returns_two() {
        let report = ActiveNodesReport {
            height: 100,
            active_archives: 2,
            by_role_status: BTreeMap::new(),
        };
        assert_eq!(active_nodes_exit_code(&report, Some(3)), 2);
    }

    /// The `--require-archives` gate counts eligibility via the
    /// shared `is_active_archive` contract (`role == "ArchiveNode"
    /// && status == "Active"`), so exactly one row survives from a
    /// mixed inventory that includes the four known statuses, a
    /// future unknown status, and a Validator. The tally still shows
    /// every bucket so operators keep visibility.
    #[test]
    fn active_nodes_gate_counts_only_active_archives() {
        let nodes = vec![
            n("ArchiveNode", "Active"),
            n("ArchiveNode", "Slashed"),
            n("ArchiveNode", "Unbonding"),
            n("ArchiveNode", "Withdrawn"),
            n("ArchiveNode", "Frozen"),
            n("Validator", "Active"),
        ];
        // Contract-count uses is_active_archive.
        let contract_count = nodes.iter().filter(|r| r.is_active_archive()).count();
        assert_eq!(contract_count, 1);
        // Tally still surfaces every bucket for operator visibility.
        let tally = tally_active_nodes(&nodes);
        assert!(tally["ArchiveNode"].contains_key("Active"));
        assert!(tally["ArchiveNode"].contains_key("Slashed"));
        assert!(tally["ArchiveNode"].contains_key("Unbonding"));
        assert!(tally["ArchiveNode"].contains_key("Withdrawn"));
        assert!(tally["ArchiveNode"].contains_key("Frozen"));
        assert!(tally["Validator"].contains_key("Active"));
    }

    /// Human-format renders `key: value` lines mirroring the smoke
    /// subcommand's shape. Pinning this catches accidental
    /// formatting drift that would break operators' grep-based
    /// scripts.
    #[test]
    fn format_active_nodes_human_renders_key_value_lines() {
        let mut by = BTreeMap::new();
        let mut archive = BTreeMap::new();
        archive.insert("Active".to_string(), 3usize);
        by.insert("ArchiveNode".to_string(), archive);
        let report = ActiveNodesReport {
            height: 5_200_042,
            active_archives: 3,
            by_role_status: by,
        };
        let out = format_active_nodes_human(&report);
        assert!(out.contains("height: 5200042"), "got: {out}");
        assert!(out.contains("active_archives: 3"), "got: {out}");
        assert!(out.contains("ArchiveNode/Active: 3"), "got: {out}");
    }

    /// Empty snapshot renders the explicit "snapshot is empty"
    /// hint so a brand-new mainnet operator (zero archive nodes
    /// registered) doesn't see a silent blank line and assume the
    /// helper hung.
    #[test]
    fn format_active_nodes_human_explains_empty_snapshot() {
        let report = ActiveNodesReport {
            height: 0,
            active_archives: 0,
            by_role_status: BTreeMap::new(),
        };
        let out = format_active_nodes_human(&report);
        assert!(
            out.contains("snapshot is empty"),
            "empty-snapshot hint must appear; got: {out}"
        );
    }
}
