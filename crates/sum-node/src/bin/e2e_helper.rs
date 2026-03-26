//! E2E test helper binary for interacting with the SUM Chain L1.
//!
//! Provides CLI commands for registering nodes, registering files,
//! querying balances/blocks, and checking health — all using the same
//! `L1RpcClient` and `tx_builder` as the main `sum-node` binary.

use std::process;

use anyhow::Result;
use clap::{Parser, Subcommand};

// Import from the sum-node library crate.
use sum_node::rpc_client::L1RpcClient;
use sum_node::tx_builder;
use sum_net::identity;

#[derive(Parser)]
#[command(name = "e2e-helper", about = "E2E test helper for SUM Chain L1 interactions")]
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
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("ERROR: {e:#}");
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Health { rpc_url } => {
            let rpc = L1RpcClient::new(rpc_url.clone());
            let result: serde_json::Value = rpc.call_public("health", serde_json::json!([])).await?;
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
            let balance: serde_json::Value = rpc.call_public("get_balance", serde_json::json!([address])).await?;
            println!("{balance}");
        }

        Command::BlockNumber { rpc_url } => {
            let rpc = L1RpcClient::new(rpc_url);
            let height: serde_json::Value = rpc.call_public("sum_blockNumber", serde_json::json!([])).await?;
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

        Command::RegisterNode { seed_hex, rpc_url, stake } => {
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

        Command::RegisterFile { seed_hex, rpc_url, merkle_root, total_size, fee_deposit } => {
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
                &seed, chain_id, nonce, 1_000_000,
                root, total_size, vec![], fee_deposit,
            )?;

            let result = rpc.send_raw_transaction(&tx_hex).await?;
            println!("Submitted RegisterFile tx: {result}");
        }
    }
    Ok(())
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
