//! SUM Storage Node binary — decentralized file storage daemon.
//!
//! ```bash
//! # Listen + serve chunks + respond to PoR challenges
//! RUST_LOG=info cargo run --bin sum-node -- listen
//!
//! # Ingest a file, store chunks, announce on mesh
//! RUST_LOG=info cargo run --bin sum-node -- ingest path/to/file.bin
//!
//! # Fetch a chunk by CID from a LAN peer
//! RUST_LOG=info cargo run --bin sum-node -- fetch <cid>
//!
//! # Send a test Gossipsub message
//! RUST_LOG=info cargo run --bin sum-node -- send "Hello from SUM Node"
//! ```

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use sum_net::{SumNet, SumNetEvent, Keypair, ShardResponse, TOPIC_STORAGE, TOPIC_TEST};
use sum_net::identity;
use sum_store::{SumStore, FetchOutcome, decode_announcement};
use sum_store::manifest::deserialize_manifest_cbor;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;
use sum_types::config::{NetConfig, StoreConfig};

use sum_node::acl::AclChecker;
use sum_node::assignment_attestor::AssignmentAttestor;
use sum_node::download::DownloadOrchestrator;
use sum_node::download_private::run_download_private;
use sum_node::download_route::{route_download_target, DownloadPath};
use sum_node::inbound_v2::V2Dispatcher;
use sum_node::market_sync::MarketSyncWorker;
use sum_node::peer_state::{apply_peer_event, PeerMapChange};
use sum_node::por_worker::PorWorker;
use sum_node::profile::{log_profile_banner, NodeProfile};
use sum_node::push_validator::{PushValidator, V2Params};
use sum_node::rpc_client::L1RpcClient;
use sum_node::upload::UploadOrchestrator;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "sum-node",
    version = env!("CARGO_PKG_VERSION"),
    about   = "SUM Storage Node — decentralized file storage"
)]
struct Cli {
    /// Path to Ed25519 private key seed file (32 bytes, hex-encoded).
    /// If not provided, a random keypair is generated (dev mode).
    #[arg(long, env = "SUM_KEY_FILE")]
    key_file: Option<PathBuf>,

    /// URL of the SUM Chain L1 JSON-RPC endpoint.
    #[arg(long, env = "SUM_RPC_URL", default_value = "http://127.0.0.1:9944")]
    rpc_url: String,

    /// PoR challenge poll interval in seconds.
    #[arg(long, env = "SUM_POR_INTERVAL", default_value = "10")]
    por_poll_secs: u64,

    /// Market sync poll interval in seconds.
    #[arg(long, env = "SUM_MARKET_SYNC_INTERVAL", default_value = "30")]
    market_sync_secs: u64,

    /// Garbage collection grace period in seconds.
    /// Unassigned chunks are kept for this long before deletion.
    #[arg(long, env = "SUM_GC_GRACE", default_value = "3600")]
    gc_grace_secs: u64,

    /// SUM Chain V2 chain ID. Used by V2 tx builders (`AcceptAssignmentV2`,
    /// etc.) when signing on-chain transactions.
    #[arg(long, env = "SUM_CHAIN_ID", default_value = "1337")]
    chain_id: u64,

    /// Per-tx fee for V2 attestation (`AcceptAssignmentV2`). Phase 0b
    /// uses a static value; W10 may parameterize per-call.
    #[arg(long, env = "SUM_ATTEST_FEE", default_value = "1000000")]
    attest_fee: u128,

    /// Run in client mode (upload/download only, no storage node services).
    /// In client mode: no PorWorker, no MarketSync, no GC, no chunk serving.
    /// The ingest command pushes to R=3 nodes and exits after confirmation.
    /// The listen command is not available in client mode.
    #[arg(long, env = "SUM_CLIENT_MODE")]
    client: bool,

    /// Enable WAN discovery via Kademlia DHT + TCP transport.
    /// When disabled (default), only mDNS (LAN) discovery is used.
    #[arg(long, env = "SUM_ENABLE_WAN")]
    enable_wan: bool,

    /// Bootstrap peer multiaddrs for Kademlia DHT (repeatable or comma-separated).
    /// Example: /ip4/1.2.3.4/tcp/4001/p2p/12D3KooW...
    #[arg(long = "bootstrap-peer", env = "SUM_BOOTSTRAP_PEERS", value_delimiter = ',')]
    bootstrap_peers: Vec<String>,

    /// TCP listen port for WAN connections (0 = OS-assigned).
    #[arg(long, env = "SUM_TCP_PORT", default_value = "0")]
    tcp_port: u16,

    /// UDP listen port for the QUIC transport (0 = OS-assigned).
    ///
    /// Pin a stable port if you need this node to be reliably dialable
    /// over QUIC from the WAN — e.g. behind UPnP UDP port-forward, or
    /// to give DCUtR a fixed hole-punch target. With the default `0`
    /// the OS picks a fresh ephemeral UDP port on every restart.
    #[arg(long, env = "SUM_UDP_PORT", default_value = "0")]
    udp_port: u16,

    /// Volunteer this node as a Circuit Relay v2 server.
    /// Only enable on publicly-reachable hosts (VPS, port-forwarded).
    /// Requires --enable-wan; otherwise the relay is unreachable.
    #[arg(long, env = "SUM_RELAY_SERVER")]
    relay_server: bool,

    /// Runtime profile. `production` fails closed on every uncertain ACL path
    /// (RPC errors, unregistered files, unknown CIDs all deny). `dev` relaxes
    /// those paths for local testing without an L1 chain — never use it for
    /// real deployments.
    #[arg(long, env = "SUM_PROFILE", default_value = "production", value_enum)]
    profile: NodeProfile,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen indefinitely, serving chunk requests, enforcing ACLs,
    /// and responding to PoR challenges.
    Listen,

    /// Ingest a file: chunk, push to R=3 assigned nodes, announce on mesh.
    Ingest {
        /// Path to the file to ingest.
        path: PathBuf,
        /// Upload timeout in seconds (time to wait for R=3 push confirmations).
        #[arg(long, default_value = "120")]
        upload_timeout_secs: u64,
        /// Manifest-replication timeout in seconds. Ingest only declares
        /// success after every chunk recipient ACKs the manifest push,
        /// so they can resolve `cid -> merkle_root` for ACL purposes.
        /// Manifests are KB-scale; 60 s is generous default headroom.
        #[arg(long, default_value = "60")]
        manifest_push_timeout_secs: u64,
    },

    /// V2 ingest a file via the chain plan v3.2 lifecycle:
    /// `RegisterFilePendingV2` → push chunks → push manifest →
    /// poll coverage → `ActivateFileV2`. On success the file is
    /// `Active` on chain. On post-register failure the file is left
    /// `Pending`; operator can `resume` (W11) to retry or `abandon`
    /// (W11) to release the deposit.
    IngestV2 {
        /// Path to the file to ingest.
        path: PathBuf,
        /// Wall-clock timeout for the S2 push wave.
        #[arg(long, default_value = "120")]
        push_wait_secs: u64,
        /// Wall-clock timeout for the S3 manifest-push wave.
        #[arg(long, default_value = "60")]
        manifest_push_wait_secs: u64,
        /// Wall-clock timeout for S4 coverage polling. NOT
        /// `activation_grace_blocks` — that's a chain-side parameter
        /// for `AbandonFileV2` admissibility, not for ingest progression.
        #[arg(long, default_value = "300")]
        activation_wait_secs: u64,
        /// File visibility on chain. `public` (default) leaves chunks /
        /// manifest unencrypted; `private` (Phase 4a) generates a fresh
        /// `K_file`, encrypts every chunk + the manifest, and wraps
        /// `K_file` for each recipient (the owner is auto-added).
        #[arg(long, default_value = "public", value_parser = ["public", "private"])]
        visibility: String,
        /// Recipient for a Private file: `<base58 L1 addr>` or
        /// `<base58 L1 addr>:<expires_at_height>`. Repeatable. The owner
        /// is auto-added; supplying recipients here makes the file
        /// "owner-shared". Each recipient's X25519 pubkey is fetched
        /// from chain via `account_getEncryptionPublicKey` — recipients
        /// without a registered key cause ingest to abort BEFORE any
        /// chain state is created.
        #[arg(long = "recipient")]
        recipients: Vec<String>,
    },

    /// Fetch a chunk by CID from a LAN peer.
    Fetch {
        /// CIDv1 string of the chunk to fetch.
        cid: String,
    },

    /// Download a complete file by merkle root from the network.
    Download {
        /// Hex-encoded merkle root of the file to download.
        merkle_root: String,
        /// Path to write the reassembled file.
        #[arg(long)]
        output: PathBuf,
        /// Maximum concurrent chunk fetches.
        #[arg(long, default_value = "10")]
        max_concurrent: usize,
        /// Download timeout in seconds.
        #[arg(long, default_value = "300")]
        download_timeout_secs: u64,
    },

    /// V2 resume: re-run the post-register portion of the V2 lifecycle
    /// against a `Pending` file (chain plan v3.2). Caller MUST pass
    /// both the explicit `merkle_root` (recorded from the prior
    /// `IngestV2` outcome) AND the original file path. Mismatch
    /// surfaces as a typed `RootMismatch` outcome.
    Resume {
        /// Hex-encoded 32-byte merkle root of the pending file.
        merkle_root: String,
        /// Path to the original file (re-chunked locally to rebuild
        /// Merkle proofs for any missing pushes).
        path: PathBuf,
        /// Wall-clock timeout for the resume's S2 partial push wave.
        #[arg(long, default_value = "120")]
        push_wait_secs: u64,
        /// Wall-clock timeout for the resume's S3 manifest re-push.
        #[arg(long, default_value = "60")]
        manifest_push_wait_secs: u64,
        /// Wall-clock timeout for the resume's S4 coverage poll.
        #[arg(long, default_value = "300")]
        activation_wait_secs: u64,
    },

    /// V2 abandon: submit `AbandonFileV2` for a `Pending` file we own.
    /// Pre-checks chain state to give a clean "wait until height N"
    /// message before burning a tx fee against chain plan §3.5
    /// strict-`>` grace rule.
    Abandon {
        /// Hex-encoded 32-byte merkle root of the pending file.
        merkle_root: String,
    },

    /// Phase 4a — register an X25519 encryption public key on chain so
    /// other accounts can wrap private-file `K_file`s for this account.
    ///
    /// Derives the X25519 keypair deterministically from the Ed25519 seed
    /// in `--key-file` via HKDF (domain `snip-x25519-encryption-key-v1`),
    /// signs `NodeRegistryV2::RegisterEncryptionKey`, submits, and waits
    /// for finality. Re-runs are idempotent (chain overwrites the slot).
    RegisterEncryptionKey,

    /// Phase 4c — share a Private V2 file with a new recipient.
    /// Owner-only: recovers `K_file` locally from the owner's own
    /// access bundle on chain, wraps it for the new recipient's
    /// registered X25519 key, and submits `AddAccessV2`. The chain
    /// never sees `K_file`.
    Share {
        /// Hex-encoded 32-byte merkle root of the file.
        merkle_root: String,
        /// Recipient: `<base58 L1 addr>` or `<addr>:<expires_at_height>`
        /// or `<addr>:none`. The recipient's X25519 pubkey is fetched
        /// from chain via `account_getEncryptionPublicKey`; missing
        /// keys cause `share` to abort BEFORE submitting any tx.
        #[arg(long)]
        recipient: String,
    },

    /// Phase 4c — revoke a recipient's access to a Private V2 file.
    /// Owner-only. Removes the chain-side access entry. Does NOT
    /// rotate `K_file`: the revoked recipient still holds their old
    /// bundle locally but chain ACL denies them on the next pull.
    /// For forward secrecy, revoke + re-ingest under a fresh key.
    Revoke {
        /// Hex-encoded 32-byte merkle root of the file.
        merkle_root: String,
        /// Address to revoke. The expiry segment (if any) is ignored
        /// for revoke.
        #[arg(long)]
        recipient: String,
    },

    /// Phase 4c — update a recipient's expiry on a Private V2 file.
    /// Owner-only. Preserves the existing encrypted_key_bundle
    /// byte-for-byte; only the entry's `expires_at` changes.
    /// REQUIRES an explicit expiry directive: `<addr>:<height>` to
    /// set, `<addr>:none` to clear. A bare `<addr>` is rejected as
    /// no-op so the operator's intent is unambiguous.
    UpdateAccess {
        /// Hex-encoded 32-byte merkle root of the file.
        merkle_root: String,
        /// Recipient + explicit expiry directive (`:<height>` or `:none`).
        #[arg(long)]
        recipient: String,
    },

    /// Discover a peer on the LAN, publish a test message, then exit.
    Send {
        /// UTF-8 message to broadcast on `sum/test/v1`.
        message: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    log_profile_banner(cli.profile);

    // Load or generate keypair + extract seed.
    let (keypair, seed) = if let Some(ref key_path) = cli.key_file {
        let hex_str = std::fs::read_to_string(key_path)?
            .trim()
            .to_string();
        let seed_bytes = hex_to_bytes_32(&hex_str)?;
        let kp = identity::keypair_from_seed(&seed_bytes)?;
        let peer_id = identity::peer_id_from_keypair(&kp);
        let l1_addr = identity::l1_address_from_keypair(&kp);
        info!(
            %peer_id,
            l1_address = %identity::l1_address_hex(&l1_addr),
            l1_base58 = %identity::l1_address_base58(&l1_addr),
            "loaded L1 keypair"
        );
        (kp, Some(seed_bytes))
    } else {
        warn!("no --key-file provided — generating random keypair (dev mode, PoR disabled)");
        (Keypair::generate_ed25519(), None)
    };

    if cli.relay_server && !cli.enable_wan {
        warn!("--relay-server has no effect without --enable-wan; the relay will not be reachable over the DHT");
    }

    // Build network config from CLI args.
    let net_config = NetConfig {
        udp_listen_port: cli.udp_port,
        tcp_listen_port: cli.tcp_port,
        enable_wan: cli.enable_wan,
        bootstrap_peers: cli.bootstrap_peers.clone(),
        relay_server: cli.relay_server,
    };

    match cli.command {
        Command::Listen if cli.client => {
            anyhow::bail!("listen command requires node mode — remove --client flag")
        }
        Command::Listen => run_listen(keypair, seed, &cli, net_config).await,
        Command::Ingest {
            path,
            upload_timeout_secs,
            manifest_push_timeout_secs,
        } => {
            let rpc_url = cli.rpc_url.clone();
            let client_mode = cli.client;
            let profile = cli.profile;
            run_ingest(
                keypair,
                rpc_url,
                client_mode,
                net_config,
                path,
                upload_timeout_secs,
                manifest_push_timeout_secs,
                profile,
            )
            .await
        }
        Command::IngestV2 {
            path,
            push_wait_secs,
            manifest_push_wait_secs,
            activation_wait_secs,
            visibility,
            recipients,
        } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!(
                    "ingest-v2 requires --key-file (V2 register/activate transactions need a signing key)"
                )
            })?;
            // Reject `--recipient` on Public ingest BEFORE any chain
            // RPC — passing it would silently be ignored otherwise.
            if visibility == "public" && !recipients.is_empty() {
                anyhow::bail!(
                    "--recipient is meaningful only with --visibility private; got {} recipients on a public ingest",
                    recipients.len()
                );
            }
            run_ingest_v2(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                cli.profile,
                net_config,
                path,
                push_wait_secs,
                manifest_push_wait_secs,
                activation_wait_secs,
                visibility,
                recipients,
            )
            .await
        }
        Command::Resume {
            merkle_root,
            path,
            push_wait_secs,
            manifest_push_wait_secs,
            activation_wait_secs,
        } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!("resume requires --key-file (V2 ActivateFileV2 needs a signing key)")
            })?;
            run_resume_v2(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                cli.profile,
                net_config,
                merkle_root,
                path,
                push_wait_secs,
                manifest_push_wait_secs,
                activation_wait_secs,
            )
            .await
        }
        Command::Abandon { merkle_root } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!("abandon requires --key-file (AbandonFileV2 needs a signing key)")
            })?;
            run_abandon_v2(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                cli.profile,
                net_config,
                merkle_root,
            )
            .await
        }
        Command::RegisterEncryptionKey => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!(
                    "register-encryption-key requires --key-file (NodeRegistryV2 needs a signing key)"
                )
            })?;
            run_register_encryption_key(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
            )
            .await
        }
        Command::Share { merkle_root, recipient } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!("share requires --key-file (owner key recovers K_file locally)")
            })?;
            let parsed_root = parse_merkle_root_hex(&merkle_root)?;
            let recipient_spec = sum_node::access::parse_recipient_spec(&recipient)?;
            sum_node::access::run_share(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                parsed_root,
                recipient_spec,
            )
            .await
        }
        Command::Revoke { merkle_root, recipient } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!("revoke requires --key-file (RemoveAccessV2 needs a signing key)")
            })?;
            let parsed_root = parse_merkle_root_hex(&merkle_root)?;
            let target = sum_node::access::parse_recipient_spec(&recipient)?;
            sum_node::access::run_revoke(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                parsed_root,
                target,
            )
            .await
        }
        Command::UpdateAccess { merkle_root, recipient } => {
            let seed = seed.ok_or_else(|| {
                anyhow::anyhow!("update-access requires --key-file (UpdateAccessV2 needs a signing key)")
            })?;
            let parsed_root = parse_merkle_root_hex(&merkle_root)?;
            let target = sum_node::access::parse_recipient_spec(&recipient)?;
            sum_node::access::run_update_access(
                keypair,
                seed,
                cli.rpc_url.clone(),
                cli.chain_id,
                cli.attest_fee,
                parsed_root,
                target,
            )
            .await
        }
        Command::Fetch { cid } => run_fetch(keypair, net_config, cid).await,
        Command::Download { merkle_root, output, max_concurrent, download_timeout_secs } => {
            run_download(keypair, seed, cli.rpc_url.clone(), net_config, merkle_root, output, max_concurrent, download_timeout_secs).await
        }
        Command::Send { message } => run_send(keypair, net_config, message).await,
    }
}

/// Parse a 64-char hex string into a 32-byte array.
fn hex_to_bytes_32(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        anyhow::bail!(
            "key file must contain exactly 64 hex characters (32 bytes), got {}",
            hex.len()
        );
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk)?;
        bytes[i] = u8::from_str_radix(s, 16)?;
    }
    Ok(bytes)
}

// ── Listen mode ───────────────────────────────────────────────────────────────

async fn run_listen(keypair: Keypair, seed: Option<[u8; 32]>, cli: &Cli, net_config: NetConfig) -> Result<()> {
    let net = Arc::new(SumNet::new(net_config, keypair.clone()).await?);
    let store = Arc::new(RwLock::new(SumStore::new(StoreConfig::default())?));

    // Shared PeerId -> L1 Address map (populated by PeerIdentified events).
    let peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // RPC client.
    let rpc = Arc::new(L1RpcClient::new(cli.rpc_url.clone()));
    info!(rpc_url = %cli.rpc_url, "L1 RPC client initialized");

    // ACL checker. Profile gating decides whether RPC errors / unregistered
    // files / unknown CIDs fail-closed (production) or fail-open (dev).
    let acl = Arc::new(AclChecker::new(
        rpc.clone(),
        peer_addresses.clone(),
        cli.profile,
    ));

    // V2 dispatcher — built when we have a signing seed (chain plan v3.2
    // §3.6 receive-side). Without a seed we can't build the
    // AssignmentAttestor (needs to sign AcceptAssignmentV2), so dev-mode
    // nodes skip V2 inbound entirely. ShardRequestedV2 events will surface
    // but be logged-only.
    let mut v2_dispatcher: Option<Arc<V2Dispatcher<L1RpcClient, L1RpcClient, L1RpcClient>>> = None;

    // Spawn PoR worker if we have an L1 keypair.
    // Shutdown signal for background workers.
    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);

    if let Some(seed) = seed {
        let l1_addr = identity::l1_address_from_keypair(&keypair);
        let l1_base58 = identity::l1_address_base58(&l1_addr);
        let por = PorWorker::new(
            rpc.clone(),
            seed,
            l1_base58.clone(),
            Duration::from_secs(cli.por_poll_secs),
        );
        let store_clone = store.clone();
        let por_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move { por.run(store_clone, por_shutdown).await });
        // Spawn MarketSync worker
        let market_sync = MarketSyncWorker::new(
            rpc.clone(),
            l1_addr,
            l1_base58.clone(),
            Duration::from_secs(cli.market_sync_secs),
            Duration::from_secs(cli.gc_grace_secs),
        );
        let store_clone2 = store.clone();
        let net_clone = net.clone();
        let peer_addrs_clone = peer_addresses.clone();
        let market_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            market_sync.run(store_clone2, net_clone, peer_addrs_clone, market_shutdown).await;
        });

        // V2 dispatcher (chain plan v3.2 §3.6 receive-side). The
        // dispatcher and the V1 handler share the same `Arc<RwLock<SumStore>>`
        // so V1+V2 traffic land in the same on-disk store under the
        // same lock — no split-brain between protocol versions.
        //
        // `chain_id` and `fee_per_tx` are placeholders until W10 wires
        // them from the per-tx context; for now we use the same values
        // PoR/MarketSync use. `chain_getChainParams` will replace
        // `V2Params::DEFAULTS` in W10 if the RPC lands in time.
        let validator = Arc::new(PushValidator::new(
            (*rpc).clone(),
            l1_addr,
            V2Params::DEFAULTS,
        ));
        let attestor = Arc::new(AssignmentAttestor::new(
            (*rpc).clone(),
            seed,
            l1_addr,
            cli.chain_id,
            cli.attest_fee,
            V2Params::DEFAULTS,
        ));
        // V2 reuses the same `AclChecker` as V1 — chain governs access,
        // protocol version doesn't.
        let acl_for_v2: Arc<dyn sum_node::inbound_v2::AccessChecker> = acl.clone();
        v2_dispatcher = Some(Arc::new(V2Dispatcher::new(
            validator,
            attestor,
            rpc.clone(),
            store.clone(),
            acl_for_v2,
            l1_addr,
        )));
        info!("V2 dispatcher initialized — accepting V2 ingest");
    } else {
        warn!("PoR/MarketSync workers not started — no L1 keypair available");
        warn!("V2 dispatcher not initialized — V2 ingest disabled (no signing key)");
    }

    info!("SUM Node listening — serving chunks, enforcing ACLs, press Ctrl-C to stop");

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::ShardRequested { peer_id, request, channel_id } => {
                        // Four-way dispatch:
                        //   - Manifest push (mutates manifest_idx) → write lock + handle_manifest_push.
                        //     Without this branch, archives that received chunk pushes never learn the
                        //     `cid → root` mapping, so production ACL would deny pulls to those CIDs.
                        //   - Chunk push: read lock; `serve::handle_request` already validates CID.
                        //   - Pulls: ACL gate + read lock + `handle_request`.
                        let is_manifest = request.cid.starts_with(MANIFEST_REQUEST_PREFIX);
                        let is_push = request.push_data.is_some();
                        match (is_manifest, is_push) {
                            (true, true) => {
                                let mut store_w = store.write().await;
                                sum_store::serve::handle_manifest_push(
                                    &net, &mut store_w.manifest_idx, request, *channel_id,
                                ).await;
                            }
                            (false, true) => {
                                let store_read = store.read().await;
                                sum_store::serve::handle_request(
                                    &net, &store_read.local, &store_read.manifest_idx,
                                    request, *channel_id,
                                ).await;
                            }
                            // Pull paths (manifest or chunk) — apply ACL.
                            (_, false) => {
                                let store_read = store.read().await;
                                let allowed = acl
                                    .check_access_or_default(
                                        peer_id, &request.cid, &store_read.manifest_idx,
                                    )
                                    .await;
                                if allowed {
                                    sum_store::serve::handle_request(
                                        &net, &store_read.local, &store_read.manifest_idx,
                                        request, *channel_id,
                                    ).await;
                                } else {
                                    info!(
                                        peer = %peer_id,
                                        cid = %request.cid,
                                        "ACCESS DENIED — peer not in file ACL"
                                    );
                                    let resp = ShardResponse {
                                        cid: request.cid.clone(),
                                        offset: 0,
                                        total_bytes: 0,
                                        data: Vec::new(),
                                        error: Some("ACCESS_DENIED: not in file access list".into()),
                                    };
                                    let _ = net.respond_shard(*channel_id, resp).await;
                                }
                            }
                        }
                    }
                    SumNetEvent::ShardRequestedV2 { peer_id, request, channel_id } => {
                        if let Some(ref dispatcher) = v2_dispatcher {
                            dispatcher.handle(&net, *peer_id, request.clone(), *channel_id).await;
                        } else {
                            // V2 disabled (no signing key on this node) — reply
                            // with a structurally-valid V2 error so the peer
                            // sees a clean reject rather than a timeout.
                            warn!(%peer_id, channel_id, "V2 request received but V2 dispatcher disabled — node has no signing key");
                            let resp = match request {
                                sum_net::ShardRequestV2::Pull { cid, offset, .. } => sum_net::ShardResponseV2::Data {
                                    cid: cid.clone(),
                                    offset: *offset,
                                    total_bytes: 0,
                                    data: Vec::new(),
                                    error: Some("V2 disabled on this node".into()),
                                },
                                sum_net::ShardRequestV2::Push { merkle_root, chunk_index, .. } => sum_net::ShardResponseV2::PushAck {
                                    merkle_root: *merkle_root,
                                    chunk_index: *chunk_index,
                                    error: Some("V2 disabled on this node".into()),
                                },
                                sum_net::ShardRequestV2::ManifestPush { merkle_root, .. } => sum_net::ShardResponseV2::ManifestPushAck {
                                    merkle_root: *merkle_root,
                                    error: Some("V2 disabled on this node".into()),
                                },
                                sum_net::ShardRequestV2::ManifestPull { merkle_root } => sum_net::ShardResponseV2::ManifestData {
                                    merkle_root: *merkle_root,
                                    manifest_bytes: Vec::new(),
                                    error: Some("V2 disabled on this node".into()),
                                },
                            };
                            let _ = net.respond_shard_v2(*channel_id, resp).await;
                        }
                    }
                    SumNetEvent::ShardReceivedV2 { .. } => {
                        // Phase 0b listen-mode doesn't initiate V2 outbound
                        // requests (W10 will wire that). Just trace.
                        print_event(&event);
                    }
                    SumNetEvent::PeerIdentified { .. } | SumNetEvent::PeerDisconnected { .. } => {
                        let mut map = peer_addresses.write().await;
                        match apply_peer_event(&mut map, &event) {
                            PeerMapChange::Inserted => {
                                if let SumNetEvent::PeerIdentified { peer_id, l1_address } = &event {
                                    info!(
                                        peer = %peer_id,
                                        l1 = %identity::l1_address_base58(l1_address),
                                        "peer L1 identity mapped"
                                    );
                                }
                            }
                            PeerMapChange::Removed => {
                                if let SumNetEvent::PeerDisconnected { peer_id } = &event {
                                    debug!(peer = %peer_id, "removed disconnected peer from identity map");
                                }
                                print_event(&event);
                            }
                            PeerMapChange::Unchanged => {
                                print_event(&event);
                            }
                        }
                    }
                    SumNetEvent::MessageReceived { topic, data, from } => {
                        if topic == TOPIC_STORAGE {
                            if let Some(ann) = decode_announcement(data) {
                                info!(
                                    from = %from,
                                    merkle_root = %ann.merkle_root,
                                    chunk_index = ann.chunk_index,
                                    cid = %ann.chunk_cid,
                                    size = ann.size_bytes,
                                    "chunk announced"
                                );
                            }
                        }
                        print_event(&event);
                    }
                    SumNetEvent::ShardReceived { response, .. } => {
                        // Manifest responses use the "manifest:<hex>" CID convention
                        // and bypass FetchManager entirely (manifests aren't tracked
                        // there). Chunk responses go through FetchManager so the
                        // background MarketSync state machine closes its loop.
                        if let Some(root_hex) = response.cid.strip_prefix("manifest:") {
                            match deserialize_manifest_cbor(&response.data) {
                                Ok(manifest) => {
                                    let mut store_w = store.write().await;
                                    if store_w.manifest_idx.get_by_merkle_root(&manifest.merkle_root).is_none() {
                                        match store_w.manifest_idx.insert(&manifest) {
                                            Ok(()) => info!(
                                                root = %root_hex,
                                                file_name = %manifest.file_name,
                                                chunks = manifest.chunk_count,
                                                "manifest persisted by listen loop"
                                            ),
                                            Err(e) => warn!(
                                                root = %root_hex,
                                                %e,
                                                "manifest_idx.insert failed"
                                            ),
                                        }
                                    } else {
                                        debug!(root = %root_hex, "manifest already indexed — skipping");
                                    }
                                }
                                Err(e) => {
                                    warn!(root = %root_hex, %e, "manifest deserialization failed");
                                }
                            }
                        } else {
                            // Chunk response → route through FetchManager. Use the
                            // split-borrow pattern so we can hold &mut fetcher and
                            // &local from the same store guard simultaneously.
                            let mut store_guard = store.write().await;
                            let store_mut: &mut SumStore = &mut store_guard;
                            let outcome = store_mut.fetcher
                                .on_chunk_received(net.as_ref(), &store_mut.local, response)
                                .await;
                            drop(store_guard);
                            match outcome {
                                FetchOutcome::Complete { cid, size } => {
                                    info!(%cid, size, "background fetch complete");
                                }
                                FetchOutcome::InProgress => {
                                    debug!(cid = %response.cid, "background fetch in progress");
                                }
                                FetchOutcome::Failed { cid, error } => {
                                    warn!(%cid, %error, "background fetch failed");
                                }
                            }
                        }
                    }
                    _ => print_event(&event),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C — initiating graceful shutdown");
                let _ = shutdown_tx.send(true);
                tokio::time::sleep(Duration::from_secs(5)).await;
                info!("shutting down swarm");
                net.shutdown().await?;
                break;
            }
        }
    }
    Ok(())
}

// ── Ingest mode ──────────────────────────────────────────────────────────────

async fn run_ingest(
    keypair: Keypair,
    rpc_url: String,
    client_mode: bool,
    net_config: NetConfig,
    path: PathBuf,
    upload_timeout_secs: u64,
    manifest_push_timeout_secs: u64,
    profile: NodeProfile,
) -> Result<()> {
    let net = SumNet::new(net_config, keypair).await?;
    let mut store = SumStore::new(StoreConfig::default())?;

    info!(path = %path.display(), "ingesting file");
    let manifest = store.ingest_file(&path)?;

    let json = serde_json::to_string_pretty(&manifest)?;
    info!("manifest:\n{json}");

    // Wait for peer discovery + collect peer identities for the upload orchestrator.
    info!("waiting for peers on the LAN...");
    let discover_timeout = Duration::from_secs(30);
    let discover_deadline = tokio::time::Instant::now() + discover_timeout;
    let mut peer_addresses: HashMap<sum_net::PeerId, [u8; 20]> = HashMap::new();
    let mut found_peer = false;

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                print_event(&event);
                match &event {
                    SumNetEvent::PeerDiscovered { .. } => {
                        found_peer = true;
                    }
                    _ => {
                        apply_peer_event(&mut peer_addresses, &event);
                    }
                }
                // Wait a bit after first peer to collect more identities
                if found_peer && peer_addresses.is_empty() {
                    continue; // Keep waiting for PeerIdentified
                }
                if found_peer && !peer_addresses.is_empty() {
                    // Give a brief window for more peers to identify
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    // Drain any remaining events
                    while let Ok(Some(event)) = tokio::time::timeout(
                        Duration::from_millis(200), net.next_event()
                    ).await {
                        apply_peer_event(&mut peer_addresses, &event);
                    }
                    break;
                }
            }
            _ = tokio::time::sleep_until(discover_deadline) => {
                if !found_peer {
                    warn!("timed out waiting for peers");
                }
                break;
            }
        }
    }

    // Push to R=3 assigned nodes via UploadOrchestrator. The same RPC
    // client is reused below by the post-ingest serve loop's ACL checker.
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
    let orchestrator = UploadOrchestrator::new(
        rpc.clone(),
        Duration::from_secs(upload_timeout_secs),
    );

    info!(
        peers = peer_addresses.len(),
        "pushing chunks to assigned nodes"
    );

    let upload_result = orchestrator
        .run(&net, &store, &manifest, &peer_addresses)
        .await
        .map_err(|e| anyhow::anyhow!("upload orchestrator failed: {e}"))?;

    info!(
        confirmed = upload_result.confirmed,
        total = upload_result.total,
        chunks_fully_confirmed = upload_result.chunks_fully_confirmed,
        chunk_count = manifest.chunk_count,
        timeout = upload_result.timeout,
        failed = upload_result.failed.len(),
        "upload complete"
    );

    // Strict success criterion: every chunk must reach R replicas.
    if let Err(failure) = upload_result.check_success(sum_types::storage::REPLICATION_FACTOR)
    {
        // Surface every failed push for operator triage.
        for f in &upload_result.failed {
            warn!(chunk_index = f.chunk_index, cid = %f.cid, error = %f.error, "push failed");
        }
        // Bail loud — no gossipsub fallback. The previous behaviour silently
        // declared success after announcing on gossipsub, which let users
        // believe a file was replicated when it wasn't.
        anyhow::bail!("ingest failed: {failure}");
    }

    // Replicate the manifest to every peer that ACKed at least one
    // chunk. Without this, archives have chunks but no manifest index;
    // production ACL on chunk pulls would treat all the CIDs we just
    // successfully replicated as unknown and deny.
    //
    // This is **synchronous** — ingest does not declare success until
    // every recipient ACKs the manifest. Without that, client mode
    // could clean up the local copy while one or more archives still
    // hold chunks but no manifest, leaving the file effectively
    // unservable for those replicas.
    push_manifest_to_recipients(
        &net,
        &manifest,
        &upload_result.chunk_recipients,
        Duration::from_secs(manifest_push_timeout_secs),
    )
    .await?;

    // Announce CHUNK metadata on gossipsub. NB: this is the per-chunk
    // `ChunkAnnouncement` topic from sum-store::announce — not a
    // manifest-bytes broadcast. Other nodes use these to discover that
    // chunks exist; they still have to fetch the manifest separately.
    store.announce_chunks(&net, &manifest).await?;
    info!("chunk metadata announced via gossipsub");

    if client_mode {
        // Client mode: every chunk is at R replicas AND every replica
        // ACKed the manifest, so the local copy is safe to drop.
        info!("client mode — cleaning up local chunks");
        store.cleanup()?;
        net.shutdown().await?;
        info!("client mode — exiting");
    } else {
        // Node mode: enter serve loop with the same ACL policy as `listen`.
        info!("node mode — listening for requests (Ctrl-C to stop)");
        let peer_addrs_shared: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
            Arc::new(RwLock::new(peer_addresses));
        let acl = Arc::new(AclChecker::new(rpc, peer_addrs_shared.clone(), profile));
        let store_shared = Arc::new(RwLock::new(store));
        simple_serve_loop(&net, store_shared, acl, peer_addrs_shared).await?;
    }

    Ok(())
}

/// Push the manifest to every peer in `recipients` and wait for an ACK
/// (or error response) from each before returning.
///
/// Returns `Ok(())` only when every recipient has ACKed without error.
/// Returns an `Err` if:
///
/// * A peer's response carries a non-empty `error` field (the peer
///   rejected the manifest — e.g. CBOR decode failed or the merkle
///   root in the manifest didn't match the CID-encoded root).
/// * `timeout` elapses with one or more recipients still pending.
/// * Manifest CBOR serialization fails (programmer error).
/// * The push enqueue fails for any recipient.
///
/// Why synchronous: archives that hold chunks but lack the manifest
/// can't resolve `cid -> root`, so production ACL denies their pulls.
/// Letting client-mode cleanup or node-mode serve start before every
/// archive has the manifest leaves the file unservable from those
/// replicas. Better to surface the failure here than ship a "succeeded"
/// signal that produces unservable replicas.
async fn push_manifest_to_recipients(
    net: &SumNet,
    manifest: &sum_types::storage::DataManifest,
    recipients: &std::collections::HashSet<sum_net::PeerId>,
    timeout: Duration,
) -> Result<()> {
    if recipients.is_empty() {
        return Ok(());
    }

    // Serialize once. The CBOR shape mirrors handle_manifest_request's
    // pull-side response, so receivers can route through the same code
    // path without protocol changes.
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(manifest, &mut cbor)
        .map_err(|e| anyhow::anyhow!("manifest CBOR-serialize failed: {e}"))?;
    let root_hex: String = manifest
        .merkle_root
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let cid = format!("{MANIFEST_REQUEST_PREFIX}{root_hex}");
    let data: Arc<[u8]> = Arc::from(cbor.into_boxed_slice());

    let mut pending: HashSet<sum_net::PeerId> = recipients.clone();
    let mut rejections: HashMap<sum_net::PeerId, String> = HashMap::new();

    info!(
        recipients = pending.len(),
        bytes = data.len(),
        root = %root_hex,
        timeout_secs = timeout.as_secs(),
        "pushing manifest to chunk recipients (waiting for ACKs)"
    );

    // Send pushes. Enqueue failures are fatal — we can't have an archive
    // with chunks but no manifest, so don't pretend it's fine.
    for peer_id in recipients {
        net.push_chunk_shared(*peer_id, cid.clone(), Arc::clone(&data))
            .await
            .map_err(|e| anyhow::anyhow!("manifest push enqueue failed for {peer_id}: {e}"))?;
    }

    // Drain ACKs (or rejections) until every recipient has responded or
    // we hit the deadline. Other event types (PeerIdentified, Listening,
    // etc.) flow past untouched — ingest is past peer discovery so we
    // don't need to record them here.
    let deadline = tokio::time::Instant::now() + timeout;
    while !pending.is_empty() {
        tokio::select! {
            event = net.next_event() => {
                match event {
                    Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                        if response.cid != cid {
                            // Some other concurrent push or an out-of-order
                            // event — ignore.
                            continue;
                        }
                        if !pending.remove(&peer_id) {
                            // Either a duplicate ACK or an unrelated peer
                            // responded; nothing to do.
                            continue;
                        }
                        match response.error {
                            Some(err) => {
                                warn!(%peer_id, %err, "manifest push REJECTED by archive");
                                rejections.insert(peer_id, err);
                            }
                            None => {
                                info!(%peer_id, remaining = pending.len(),
                                    "manifest push ACKed");
                            }
                        }
                    }
                    None => {
                        anyhow::bail!(
                            "network shut down while waiting for manifest ACKs ({} pending)",
                            pending.len()
                        );
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let pending_addrs: Vec<String> =
                    pending.iter().map(|p| p.to_string()).collect();
                anyhow::bail!(
                    "manifest replication timed out: {} of {} archives have chunks but no \
                     manifest. Pending: {}",
                    pending.len(),
                    recipients.len(),
                    pending_addrs.join(", ")
                );
            }
        }
    }

    if !rejections.is_empty() {
        let detail: Vec<String> = rejections
            .iter()
            .map(|(p, e)| format!("{p}: {e}"))
            .collect();
        anyhow::bail!(
            "manifest push rejected by {} archive(s): {}",
            rejections.len(),
            detail.join("; ")
        );
    }

    info!(recipients = recipients.len(), "manifest replicated to all chunk recipients");
    Ok(())
}

/// Serve loop used after ingest announces the file. Same four-way
/// dispatch as `run_listen`: manifest pushes take a write lock, chunk
/// pushes use a read lock with CID-only validation, pulls go through
/// the ACL checker.
async fn simple_serve_loop(
    net: &SumNet,
    store: Arc<RwLock<SumStore>>,
    acl: Arc<AclChecker>,
    peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>>,
) -> Result<()> {
    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::ShardRequested { peer_id, request, channel_id } => {
                        let is_manifest = request.cid.starts_with(MANIFEST_REQUEST_PREFIX);
                        let is_push = request.push_data.is_some();
                        match (is_manifest, is_push) {
                            (true, true) => {
                                let mut store_w = store.write().await;
                                sum_store::serve::handle_manifest_push(
                                    net, &mut store_w.manifest_idx, request, *channel_id,
                                ).await;
                            }
                            (false, true) => {
                                let store_read = store.read().await;
                                sum_store::serve::handle_request(
                                    net, &store_read.local, &store_read.manifest_idx,
                                    request, *channel_id,
                                ).await;
                            }
                            (_, false) => {
                                let store_read = store.read().await;
                                let allowed = acl
                                    .check_access_or_default(
                                        peer_id, &request.cid, &store_read.manifest_idx,
                                    )
                                    .await;
                                if allowed {
                                    sum_store::serve::handle_request(
                                        net, &store_read.local, &store_read.manifest_idx,
                                        request, *channel_id,
                                    ).await;
                                } else {
                                    info!(
                                        peer = %peer_id,
                                        cid = %request.cid,
                                        "ACCESS DENIED — peer not in file ACL"
                                    );
                                    let resp = ShardResponse {
                                        cid: request.cid.clone(),
                                        offset: 0,
                                        total_bytes: 0,
                                        data: Vec::new(),
                                        error: Some("ACCESS_DENIED: not in file access list".into()),
                                    };
                                    let _ = net.respond_shard(*channel_id, resp).await;
                                }
                            }
                        }
                    }
                    SumNetEvent::PeerIdentified { .. } | SumNetEvent::PeerDisconnected { .. } => {
                        let mut map = peer_addresses.write().await;
                        let _ = apply_peer_event(&mut map, &event);
                        print_event(&event);
                    }
                    _ => print_event(&event),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C — shutting down");
                net.shutdown().await?;
                break;
            }
        }
    }
    Ok(())
}

// ── V2 Ingest mode (chain plan v3.2) ────────────────────────────────────────
//
// Wires the W10a `IngestPipeline` into a CLI subcommand. CLI-only glue:
// spin up SumNet, run the existing peer-discovery loop, build a
// `MapPeerResolver`, populate `IngestParams` from
// `chain_get_chain_params` (profile-aware fallback), construct the
// pipeline, run, print outcome.

#[allow(clippy::too_many_arguments)]
async fn run_ingest_v2(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id_arg: u64,
    fee_per_tx: u128,
    profile: NodeProfile,
    net_config: NetConfig,
    path: PathBuf,
    push_wait_secs: u64,
    manifest_push_wait_secs: u64,
    activation_wait_secs: u64,
    visibility: String,
    recipient_specs: Vec<String>,
) -> Result<()> {
    let l1_addr = identity::l1_address_from_keypair(&keypair);
    let net = Arc::new(SumNet::new(net_config, keypair).await?);
    let rpc = Arc::new(L1RpcClient::new(rpc_url));

    // ── Peer discovery (same pattern as V1 run_ingest) ──────────────
    info!("waiting for peers on the LAN...");
    let peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let discover_timeout = Duration::from_secs(30);
    let discover_deadline = tokio::time::Instant::now() + discover_timeout;
    let mut found_peer = false;
    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                print_event(&event);
                match &event {
                    SumNetEvent::PeerDiscovered { .. } => {
                        found_peer = true;
                    }
                    _ => {
                        let mut map = peer_addresses.write().await;
                        apply_peer_event(&mut map, &event);
                    }
                }
                let map_empty = peer_addresses.read().await.is_empty();
                if found_peer && map_empty {
                    continue;
                }
                if found_peer && !map_empty {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while let Ok(Some(event)) = tokio::time::timeout(
                        Duration::from_millis(200), net.next_event()
                    ).await {
                        let mut map = peer_addresses.write().await;
                        apply_peer_event(&mut map, &event);
                    }
                    break;
                }
            }
            _ = tokio::time::sleep_until(discover_deadline) => {
                if !found_peer {
                    warn!("timed out waiting for peers");
                }
                break;
            }
        }
    }
    info!(
        peer_count = peer_addresses.read().await.len(),
        "peer discovery complete; entering V2 ingest"
    );

    // ── Build IngestParams ─────────────────────────────────────────
    //
    // Profile-aware: production must read `chain_getChainParams` and
    // fail loudly on RPC error (otherwise we'd silently bake stale
    // defaults into a real-money path). Dev allows fallback to
    // hardcoded defaults with a `warn!` so local testing without a
    // chain stays unblocked.
    let defaults = sum_node::ingest_v2::IngestParamsDefaults {
        poll_interval: Duration::from_secs(2),
        activation_wait_secs: Duration::from_secs(activation_wait_secs),
        finality_timeout: Duration::from_secs(60),
        push_retries: 2,
        push_wait_secs: Duration::from_secs(push_wait_secs),
        manifest_push_wait_secs: Duration::from_secs(manifest_push_wait_secs),
    };
    let params = match rpc.chain_get_chain_params().await {
        Ok(cp) => {
            info!(
                chain_id = cp.chain_id,
                R = cp.assignment_replication_factor,
                max_chunk_indices = cp.max_chunk_indices_per_tx,
                grace = cp.activation_grace_blocks,
                "chain_getChainParams: live values populated into IngestParams"
            );
            sum_node::ingest_v2::IngestParams::from_chain_params(&cp, defaults)
        }
        Err(e) => match profile {
            NodeProfile::Production => {
                anyhow::bail!(
                    "production profile requires chain_getChainParams to succeed before ingest; \
                     refusing to fall back to defaults: {e}"
                );
            }
            NodeProfile::Dev => {
                warn!(
                    %e,
                    "chain_getChainParams failed in dev profile; falling back to V2Params::DEFAULTS — \
                     these MUST NOT be used against a real chain"
                );
                sum_node::ingest_v2::IngestParams {
                    assignment_replication_factor: 3,
                    max_chunk_indices_per_tx: 65_536,
                    max_chunk_count_per_file: 1_048_576,
                    chain_id: chain_id_arg,
                    fee_per_tx,
                    poll_interval: defaults.poll_interval,
                    activation_wait_secs: defaults.activation_wait_secs,
                    finality_timeout: defaults.finality_timeout,
                    push_retries: defaults.push_retries,
                    push_wait_secs: defaults.push_wait_secs,
                    manifest_push_wait_secs: defaults.manifest_push_wait_secs,
                    activation_grace_blocks: 50,
                    // Dev fallback assumes V2 is enabled from genesis
                    // (matches the local-mirror profile). Any production
                    // chain will provide this via chain_getChainParams,
                    // and `None` there means V2 disabled — caller's gate
                    // refuses tx submission in that case.
                    v2_enabled_from_height: Some(0),
                }
            }
        },
    };

    // ── Construct pipeline ─────────────────────────────────────────
    let resolver = Arc::new(sum_node::ingest_v2::MapPeerResolver::new(peer_addresses));
    let pipeline = sum_node::ingest_v2::IngestPipeline::new(
        rpc.clone(),
        net.clone(),
        resolver,
        seed,
        l1_addr,
        params,
    );

    // ── Run (Public or Private dispatch) ───────────────────────────
    let outcome = match visibility.as_str() {
        "public" => pipeline.run(&path).await,
        "private" => {
            let spec = build_private_ingest_spec(&rpc, &seed, l1_addr, &recipient_specs).await?;
            pipeline.run_private(&path, spec).await
        }
        // Unreachable: clap value_parser already restricts to {public, private}.
        other => anyhow::bail!("unsupported --visibility {other}"),
    };
    match &outcome {
        sum_node::ingest_v2::IngestOutcome::Activated {
            merkle_root,
            register_tx_hash,
            register_height,
            activate_tx_hash,
            activate_height,
            ..
        } => {
            info!(
                root = %hex::encode(merkle_root),
                %register_tx_hash,
                register_height,
                %activate_tx_hash,
                activate_height,
                "V2 ingest ACTIVATED"
            );
        }
        sum_node::ingest_v2::IngestOutcome::Failed { stage, source, .. } => {
            warn!(?stage, %source, "V2 ingest FAILED before chain registration");
        }
        sum_node::ingest_v2::IngestOutcome::PendingNeedsAction {
            merkle_root,
            failed_stage,
            under_replicated_chunks,
            suggested,
            source,
            ..
        } => {
            warn!(
                root = %hex::encode(merkle_root),
                ?failed_stage,
                ?suggested,
                under_replicated_count = under_replicated_chunks.as_ref().map(|v| v.len()).unwrap_or(0),
                source = ?source.as_ref().map(|e| e.to_string()),
                "V2 ingest PENDING — file is registered on chain; run `resume` or `abandon`"
            );
        }
        // Resume-only outcomes: cannot actually be produced by `run`,
        // but the match must be exhaustive over the unified enum.
        sum_node::ingest_v2::IngestOutcome::ActivatedOnChain { merkle_root, register_height, activate_height, .. } => {
            info!(
                root = %hex::encode(merkle_root),
                register_height,
                activate_height,
                "V2 ingest: file already ACTIVE on chain (no-op)"
            );
        }
        sum_node::ingest_v2::IngestOutcome::ResumedActivated {
            merkle_root, register_height, activate_tx_hash, activate_height, ..
        } => {
            info!(
                root = %hex::encode(merkle_root),
                register_height,
                %activate_tx_hash,
                activate_height,
                "V2 resume ACTIVATED (no original register_tx_hash on chain)"
            );
        }
        sum_node::ingest_v2::IngestOutcome::AbandonedOnChain { merkle_root, abandoned_at_height, .. } => {
            warn!(
                root = %hex::encode(merkle_root),
                ?abandoned_at_height,
                "V2 ingest: file is ABANDONED on chain (terminal)"
            );
        }
        sum_node::ingest_v2::IngestOutcome::RootMismatch { expected, actual, .. } => {
            warn!(
                expected = %hex::encode(expected),
                actual = %hex::encode(actual),
                "V2 ingest: root mismatch — file path doesn't match recorded merkle_root"
            );
        }
    }
    net.shutdown().await?;
    Ok(())
}

// ── V2 Resume + Abandon (W11) ───────────────────────────────────────────────

fn parse_merkle_root_hex(s: &str) -> Result<[u8; 32]> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped)
        .map_err(|e| anyhow::anyhow!("invalid merkle_root hex: {e}"))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "merkle_root must be 32 bytes (got {} bytes)",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Build `IngestParams` with profile-aware fallback. Shared by
/// `run_ingest_v2`, `run_resume_v2`, and `run_abandon_v2`.
async fn build_v2_ingest_params(
    rpc: &Arc<L1RpcClient>,
    profile: NodeProfile,
    chain_id_arg: u64,
    fee_per_tx: u128,
    defaults: sum_node::ingest_v2::IngestParamsDefaults,
) -> Result<sum_node::ingest_v2::IngestParams> {
    match rpc.chain_get_chain_params().await {
        Ok(cp) => {
            info!(
                chain_id = cp.chain_id,
                R = cp.assignment_replication_factor,
                grace = cp.activation_grace_blocks,
                "chain_getChainParams: live values populated into IngestParams"
            );
            Ok(sum_node::ingest_v2::IngestParams::from_chain_params(&cp, defaults))
        }
        Err(e) => match profile {
            NodeProfile::Production => {
                anyhow::bail!(
                    "production profile requires chain_getChainParams to succeed; \
                     refusing to fall back to defaults: {e}"
                );
            }
            NodeProfile::Dev => {
                warn!(%e, "chain_getChainParams failed in dev profile; using V2Params::DEFAULTS");
                Ok(sum_node::ingest_v2::IngestParams {
                    assignment_replication_factor: 3,
                    max_chunk_indices_per_tx: 65_536,
                    max_chunk_count_per_file: 1_048_576,
                    chain_id: chain_id_arg,
                    fee_per_tx,
                    poll_interval: defaults.poll_interval,
                    activation_wait_secs: defaults.activation_wait_secs,
                    finality_timeout: defaults.finality_timeout,
                    push_retries: defaults.push_retries,
                    push_wait_secs: defaults.push_wait_secs,
                    manifest_push_wait_secs: defaults.manifest_push_wait_secs,
                    activation_grace_blocks: 50,
                    v2_enabled_from_height: Some(0),
                })
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_resume_v2(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id_arg: u64,
    fee_per_tx: u128,
    profile: NodeProfile,
    net_config: NetConfig,
    merkle_root_hex: String,
    path: PathBuf,
    push_wait_secs: u64,
    manifest_push_wait_secs: u64,
    activation_wait_secs: u64,
) -> Result<()> {
    let merkle_root = parse_merkle_root_hex(&merkle_root_hex)?;
    let l1_addr = identity::l1_address_from_keypair(&keypair);
    let net = Arc::new(SumNet::new(net_config, keypair).await?);
    let rpc = Arc::new(L1RpcClient::new(rpc_url));

    // Same peer-discovery scaffolding as run_ingest_v2 — resume needs
    // peers for the partial S2 push wave.
    info!("waiting for peers on the LAN...");
    let peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let discover_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut found_peer = false;
    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                print_event(&event);
                match &event {
                    SumNetEvent::PeerDiscovered { .. } => { found_peer = true; }
                    _ => {
                        let mut map = peer_addresses.write().await;
                        apply_peer_event(&mut map, &event);
                    }
                }
                let map_empty = peer_addresses.read().await.is_empty();
                if found_peer && !map_empty {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while let Ok(Some(event)) = tokio::time::timeout(
                        Duration::from_millis(200), net.next_event()
                    ).await {
                        let mut map = peer_addresses.write().await;
                        apply_peer_event(&mut map, &event);
                    }
                    break;
                }
            }
            _ = tokio::time::sleep_until(discover_deadline) => {
                if !found_peer { warn!("timed out waiting for peers"); }
                break;
            }
        }
    }

    let defaults = sum_node::ingest_v2::IngestParamsDefaults {
        poll_interval: Duration::from_secs(2),
        activation_wait_secs: Duration::from_secs(activation_wait_secs),
        finality_timeout: Duration::from_secs(60),
        push_retries: 2,
        push_wait_secs: Duration::from_secs(push_wait_secs),
        manifest_push_wait_secs: Duration::from_secs(manifest_push_wait_secs),
    };
    let params = build_v2_ingest_params(&rpc, profile, chain_id_arg, fee_per_tx, defaults).await?;
    let resolver = Arc::new(sum_node::ingest_v2::MapPeerResolver::new(peer_addresses));
    let pipeline = sum_node::ingest_v2::IngestPipeline::new(
        rpc, net.clone(), resolver, seed, l1_addr, params,
    );

    let outcome = pipeline.resume(merkle_root, &path).await;
    match &outcome {
        sum_node::ingest_v2::IngestOutcome::Activated { activate_tx_hash, activate_height, .. } => {
            // resume never produces this variant (it'd require submitting
            // RegisterFilePendingV2 ourselves, which resume doesn't do)
            // but the match must be exhaustive.
            info!(%activate_tx_hash, activate_height, "RESUME activated file");
        }
        sum_node::ingest_v2::IngestOutcome::ResumedActivated {
            register_height, activate_tx_hash, activate_height, ..
        } => {
            info!(
                register_height,
                %activate_tx_hash,
                activate_height,
                "RESUME activated PENDING file"
            );
        }
        sum_node::ingest_v2::IngestOutcome::ActivatedOnChain { register_height, activate_height, .. } => {
            info!(register_height, activate_height, "RESUME: file already ACTIVE on chain");
        }
        sum_node::ingest_v2::IngestOutcome::AbandonedOnChain { abandoned_at_height, .. } => {
            warn!(?abandoned_at_height, "RESUME: file already ABANDONED on chain (terminal)");
        }
        sum_node::ingest_v2::IngestOutcome::RootMismatch { expected, actual, .. } => {
            warn!(
                expected = %hex::encode(expected),
                actual = %hex::encode(actual),
                "RESUME: root mismatch — wrong file for the recorded merkle_root"
            );
        }
        sum_node::ingest_v2::IngestOutcome::PendingNeedsAction { failed_stage, source, .. } => {
            warn!(?failed_stage, source = ?source.as_ref().map(|e| e.to_string()), "RESUME: still PENDING — re-run resume or abandon");
        }
        sum_node::ingest_v2::IngestOutcome::Failed { stage, source, .. } => {
            warn!(?stage, %source, "RESUME: failed");
        }
    }
    net.shutdown().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_abandon_v2(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id_arg: u64,
    fee_per_tx: u128,
    profile: NodeProfile,
    _net_config: NetConfig, // NetConfig accepted for CLI uniformity but unused — abandon is chain-only.
    merkle_root_hex: String,
) -> Result<()> {
    let merkle_root = parse_merkle_root_hex(&merkle_root_hex)?;
    let l1_addr = identity::l1_address_from_keypair(&keypair);
    // Abandon is a chain-only operation. Stand up a `NoopNet` instead
    // of a real `SumNet` so the operator can recover their fee deposit
    // even if the local libp2p stack can't bind (UDP port in use, mDNS
    // unavailable, etc). The pipeline's `abandon` path never calls
    // `push_chunk_v2` / `push_manifest_v2` / `next_event`; if a future
    // refactor accidentally does, `NoopNet` bails loudly.
    let net = Arc::new(sum_node::ingest_v2::NoopNet);
    let rpc = Arc::new(L1RpcClient::new(rpc_url));

    let defaults = sum_node::ingest_v2::IngestParamsDefaults::default();
    let params = build_v2_ingest_params(&rpc, profile, chain_id_arg, fee_per_tx, defaults).await?;
    // Empty resolver — abandon doesn't push, so peers don't matter.
    let resolver = Arc::new(sum_node::ingest_v2::MapPeerResolver::new(Arc::new(
        RwLock::new(HashMap::new()),
    )));
    // keypair would have been used to build a SumNet; threaded into
    // `_keypair` so we don't drop it unused.
    let _ = keypair;
    let pipeline = sum_node::ingest_v2::IngestPipeline::new(
        rpc, net, resolver, seed, l1_addr, params,
    );

    match pipeline.abandon(merkle_root).await {
        sum_node::ingest_v2::AbandonOutcome::Abandoned { tx_hash, finalized_at_height } => {
            info!(%tx_hash, finalized_at_height, "ABANDON finalized");
        }
        sum_node::ingest_v2::AbandonOutcome::NotAdmissible {
            reason,
            current_height,
            earliest_admissible_height,
        } => {
            warn!(
                current_height,
                earliest_admissible_height,
                %reason,
                "ABANDON not admissible — pre-check rejected"
            );
        }
        sum_node::ingest_v2::AbandonOutcome::Failed { source } => {
            warn!(%source, "ABANDON failed");
        }
    }
    Ok(())
}

// ── Private ingest helpers (Phase 4a) ─────────────────────────────────────────

/// Parse one `--recipient` value: `<addr_b58>` or
/// `<addr_b58>:<expires_at_height>`. Anything else (multiple `:`, empty
/// halves, non-base58, non-numeric expires) is rejected — silently
/// dropping a malformed recipient could surprise the operator who
/// expects shared access.
fn parse_recipient_spec(s: &str) -> Result<([u8; 20], Option<u64>)> {
    let (addr_str, expires) = match s.split_once(':') {
        Some((a, e)) => {
            let h: u64 = e
                .parse()
                .map_err(|_| anyhow::anyhow!("recipient {s:?}: expires_at must be a u64 block height"))?;
            (a, Some(h))
        }
        None => (s, None),
    };
    if addr_str.is_empty() {
        anyhow::bail!("recipient {s:?}: empty L1 address");
    }
    let addr = sum_net::l1_address_from_base58(addr_str)
        .map_err(|e| anyhow::anyhow!("recipient {s:?}: {e}"))?;
    Ok((addr, expires))
}

/// Build a [`PrivateIngestSpec`](sum_node::ingest_v2::PrivateIngestSpec)
/// from a list of CLI recipient specs. Looks up each recipient's
/// X25519 pubkey via `account_getEncryptionPublicKey` and aborts the
/// ingest if any recipient lacks a registered key — silently dropping
/// such recipients would leave the operator with a less-shared file
/// than they asked for.
async fn build_private_ingest_spec(
    rpc: &sum_node::rpc_client::L1RpcClient,
    seed: &[u8; 32],
    owner_l1_address: [u8; 20],
    specs: &[String],
) -> Result<sum_node::ingest_v2::PrivateIngestSpec> {
    use sum_node::ingest_v2::{PrivateIngestSpec, PrivateRecipient};

    let (_x25519_secret, owner_x25519_pubkey) =
        sum_crypto::x25519_keypair_from_ed25519_seed(seed);

    // Defense in depth: warn (but don't abort) if the owner's derived
    // pubkey is not registered on chain. A Private file with an
    // owner-only access list is still useful — the owner's bundle is
    // built from the *derived* pubkey here, not from chain — but if
    // the chain doesn't know about it, no other account can ever wrap
    // K_file FOR the owner from a future share. Worth surfacing.
    let owner_b58 = sum_net::l1_address_base58(&owner_l1_address);
    match rpc.account_get_encryption_public_key(&owner_b58).await {
        Ok(Some(chain_pk)) if chain_pk == owner_x25519_pubkey => {}
        Ok(Some(chain_pk)) => {
            warn!(
                addr = %owner_b58,
                derived = %hex::encode(owner_x25519_pubkey),
                on_chain = %hex::encode(chain_pk),
                "owner's derived X25519 pubkey differs from the on-chain registered key — your bundle will be sealed against the derived key, not the chain key"
            );
        }
        Ok(None) => {
            warn!(
                addr = %owner_b58,
                "owner has no encryption pubkey on chain — run `sum-node register-encryption-key` first if you want others to be able to share future Private files with you"
            );
        }
        Err(e) => warn!(addr = %owner_b58, %e, "could not pre-check owner's chain encryption key"),
    }

    let mut recipients: Vec<PrivateRecipient> = Vec::with_capacity(specs.len());
    for raw in specs {
        let (addr, expires_at) = parse_recipient_spec(raw)?;
        let addr_b58 = sum_net::l1_address_base58(&addr);
        let pk = rpc
            .account_get_encryption_public_key(&addr_b58)
            .await
            .with_context(|| format!("recipient {raw:?}: account_getEncryptionPublicKey failed"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "recipient {raw:?} ({addr_b58}) has no encryption pubkey registered on chain — \
                     ask them to run `sum-node register-encryption-key` first, or remove from --recipient"
                )
            })?;
        recipients.push(PrivateRecipient {
            l1_address: addr,
            x25519_pubkey: pk,
            expires_at,
        });
    }

    Ok(PrivateIngestSpec {
        owner_l1_address,
        owner_x25519_pubkey,
        recipients,
    })
}

// ── Register-encryption-key (Phase 4a) ───────────────────────────────────────

/// Submit `NodeRegistryV2::RegisterEncryptionKey` for this account's
/// derived X25519 pubkey, then wait for finality.
///
/// Idempotent on chain: re-running with the same seed re-derives the
/// same X25519 pubkey, so a second submission is a (fee-burning) no-op
/// at the storage layer. We still warn rather than skip if the chain
/// already has a key registered; the operator may be rotating keys (a
/// future feature), and silently dropping the tx would be surprising.
async fn run_register_encryption_key(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id: u64,
    fee: u128,
) -> Result<()> {
    use sum_node::tx_builder::build_register_encryption_key_tx;
    use sum_node::tx_wait::{wait_for_finalized, TxWaitError};

    let l1_addr = identity::l1_address_from_keypair(&keypair);
    let l1_b58 = identity::l1_address_base58(&l1_addr);
    let rpc = L1RpcClient::new(rpc_url);

    // V2-enabled gate: RegisterEncryptionKey is a NodeRegistryV2 op
    // and would be rejected (fee burned) before the chain has
    // activated V2. Read live params and the finalized head; refuse
    // up front when V2 is disabled (`v2_enabled_from_height = null`)
    // or scheduled for a future height.
    let cp = rpc.chain_get_chain_params().await?;
    let head = rpc.chain_get_block_height().await?;
    match cp.v2_enabled_from_height {
        None => anyhow::bail!(
            "V2 disabled on this chain (chain_getChainParams.v2_enabled_from_height = null) — \
             refusing to submit RegisterEncryptionKey; the chain would reject it"
        ),
        Some(enabled_at) if head.height < enabled_at => anyhow::bail!(
            "V2 not enabled yet: finalized height {} < v2_enabled_from_height {} \
             ({} blocks remaining); refusing to submit RegisterEncryptionKey — \
             the chain would burn the fee",
            head.height,
            enabled_at,
            enabled_at - head.height,
        ),
        Some(_) => {}
    }

    // Derive the X25519 pubkey deterministically from the Ed25519 seed.
    let (_x25519_secret, x25519_pubkey) =
        sum_crypto::x25519_keypair_from_ed25519_seed(&seed);

    // Read-before-write: log whether we're registering fresh or
    // overwriting. Don't fail on a mismatched-pubkey overwrite — the
    // user might be rotating a leaked secret. Do fail on RPC errors
    // other than "no key registered".
    match rpc.account_get_encryption_public_key(&l1_b58).await {
        Ok(None) => info!(addr = %l1_b58, "no encryption key on chain — registering fresh"),
        Ok(Some(existing)) if existing == x25519_pubkey => {
            info!(
                addr = %l1_b58,
                pubkey = %hex::encode(x25519_pubkey),
                "encryption key already registered with the derived pubkey — re-submitting will be a no-op overwrite (fees will still be burned)"
            );
        }
        Ok(Some(existing)) => {
            warn!(
                addr = %l1_b58,
                existing_pubkey = %hex::encode(existing),
                derived_pubkey = %hex::encode(x25519_pubkey),
                "encryption key on chain differs from the derived key — overwriting (key rotation)"
            );
        }
        Err(e) => warn!(addr = %l1_b58, err = %e, "could not pre-check encryption key on chain — proceeding anyway"),
    }

    let nonce = rpc.get_nonce(&l1_b58).await?;
    let tx_hex = build_register_encryption_key_tx(&seed, chain_id, nonce, fee, x25519_pubkey)?;
    let tx_hash_value = rpc.send_raw_transaction(&tx_hex).await?;
    let tx_hash = tx_hash_value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("send_raw_transaction returned non-string tx hash: {tx_hash_value}"))?
        .to_string();

    info!(
        %tx_hash,
        addr = %l1_b58,
        pubkey = %hex::encode(x25519_pubkey),
        "RegisterEncryptionKey submitted, waiting for finality"
    );

    let height = wait_for_finalized(
        &rpc,
        &tx_hash,
        sum_node::tx_wait::DEFAULT_POLL_INTERVAL,
        Duration::from_secs(120),
    )
    .await
    .map_err(|e| match e {
        TxWaitError::Failed { reason, block_height } => {
            anyhow::anyhow!("RegisterEncryptionKey failed at height {block_height:?}: {reason}")
        }
        TxWaitError::Dropped => anyhow::anyhow!("RegisterEncryptionKey dropped from mempool — re-run with a fresh nonce"),
        TxWaitError::Timeout { last_status } => {
            anyhow::anyhow!("RegisterEncryptionKey timed out (last status: {last_status:?})")
        }
        TxWaitError::Rpc(e) => e,
    })?;

    info!(
        %tx_hash,
        height,
        addr = %l1_b58,
        pubkey = %hex::encode(x25519_pubkey),
        "RegisterEncryptionKey finalized — this account can now receive private files"
    );
    Ok(())
}

// ── Fetch mode ───────────────────────────────────────────────────────────────

async fn run_fetch(keypair: Keypair, net_config: NetConfig, cid: String) -> Result<()> {
    let net = SumNet::new(net_config, keypair).await?;
    let mut store = SumStore::new(StoreConfig::default())?;

    if store.has_chunk(&cid) {
        info!(%cid, "chunk already exists locally");
        return Ok(());
    }

    info!(%cid, "waiting for a peer that has the chunk...");
    let timeout = Duration::from_secs(60);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut target_peer = None;

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::MessageReceived { topic, data, from } if topic == TOPIC_STORAGE => {
                        if let Some(ann) = decode_announcement(data) {
                            if ann.chunk_cid == cid {
                                info!(from = %from, "found peer with chunk");
                                target_peer = Some(*from);
                                break;
                            }
                        }
                    }
                    SumNetEvent::PeerDiscovered { peer_id, .. } => {
                        if target_peer.is_none() {
                            info!(%peer_id, "discovered peer — will request chunk");
                            target_peer = Some(*peer_id);
                            break;
                        }
                    }
                    _ => print_event(&event),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                anyhow::bail!("timed out waiting for a peer with chunk {cid}");
            }
        }
    }

    // Wait briefly for connections to establish before sending the request.
    // mDNS discovery can find a peer before the QUIC connection is ready.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let peer = target_peer.unwrap();
    info!(%peer, "sending chunk request");
    store.fetcher.start_fetch(&net, peer, cid.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let fetch_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut retries = 0;
    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match event {
                    SumNetEvent::ShardReceived { response, .. } => {
                        match store.fetcher.on_chunk_received(&net, &store.local, &response).await {
                            FetchOutcome::InProgress => {}
                            FetchOutcome::Complete { cid, size } => {
                                info!(%cid, size, "chunk fetched successfully");
                                net.shutdown().await?;
                                return Ok(());
                            }
                            FetchOutcome::Failed { cid, error } => {
                                anyhow::bail!("chunk fetch failed for {cid}: {error}");
                            }
                        }
                    }
                    SumNetEvent::ShardRequestFailed { peer_id, error } => {
                        retries += 1;
                        warn!(%error, retries, "chunk request failed — will retry");
                        if retries > 5 {
                            anyhow::bail!("chunk request failed after {retries} retries: {error}");
                        }
                        // Wait and retry with the same peer (connection may now be ready).
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        info!(%peer_id, "retrying fetch");
                        let _ = store.fetcher.start_fetch(&net, peer_id, cid.clone()).await;
                    }
                    SumNetEvent::PeerConnected { peer_id } => {
                        if !store.fetcher.is_active(&cid) {
                            info!(%peer_id, "retrying fetch with connected peer");
                            let _ = store.fetcher.start_fetch(&net, peer_id, cid.clone()).await;
                        }
                        print_event(&SumNetEvent::PeerConnected { peer_id });
                    }
                    _ => print_event(&event),
                }
            }
            _ = tokio::time::sleep_until(fetch_deadline) => {
                anyhow::bail!("timed out fetching chunk {cid}");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C — aborting fetch");
                net.shutdown().await?;
                return Ok(());
            }
        }
    }
}

// ── Download mode ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_download(
    keypair: Keypair,
    seed: Option<[u8; 32]>,
    rpc_url: String,
    net_config: NetConfig,
    merkle_root: String,
    output: PathBuf,
    max_concurrent: usize,
    timeout_secs: u64,
) -> Result<()> {
    info!(%merkle_root, output = %output.display(), "starting download");

    // ── Step 1: pre-flight chain probes BEFORE any libp2p work ─────
    //
    // We need the V2 file row to know whether this is a Private file,
    // and we want to fail fast on the obvious operator mistakes
    // (missing --key-file for a Private file) without burning a UDP
    // bind, mDNS announce, or QUIC handshake. Doing the RPC probe
    // first costs one HTTP round-trip but lets us bail before any
    // network state mutates.
    let parsed_root = parse_merkle_root_hex(&merkle_root)?;
    let root_hex = format!("0x{}", hex::encode(parsed_root));
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
    let v2_info = rpc
        .storage_get_file_info_v2(&root_hex, None, None)
        .await
        .ok();

    // Private + missing seed → bail before SumNet::new.
    if let Some(info) = v2_info.as_ref() {
        if info.visibility.is_private() && seed.is_none() {
            anyhow::bail!(
                "download of Private file {merkle_root} requires --key-file \
                 (no X25519 secret to unwrap K_file)"
            );
        }
    }

    // ── Step 2: route based on chain V2 row (fail closed on unknown
    //          visibility bytes; see download_route.rs). Routing
    //          decision must depend only on the V2 chain row presence
    //          and visibility — not on local chunk store, peer state,
    //          or any side channel.
    let path = route_download_target(v2_info.as_ref())?;

    // ── Step 3: now safe to start networking ───────────────────────
    let net = Arc::new(SumNet::new(net_config, keypair.clone()).await?);

    if matches!(path, DownloadPath::V2Private) {
        let info = v2_info.expect("V2Private implies Some(info)");
        let seed = seed.expect("checked above before SumNet::new");
        return run_download_private(
            keypair,
            seed,
            rpc,
            net,
            info,
            parsed_root,
            output,
            max_concurrent,
            Duration::from_secs(timeout_secs),
        )
        .await;
    }
    // V2Public and V1Legacy share the existing public-path orchestrator.

    // ── Step 3: existing Public / V1 / legacy path ────────────────
    let store = Arc::new(RwLock::new(SumStore::new(StoreConfig::default())?));
    let peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let orchestrator = DownloadOrchestrator::new(
        merkle_root,
        output.clone(),
        rpc,
        max_concurrent,
        Duration::from_secs(timeout_secs),
    );

    let result = orchestrator.run(net.clone(), store, peer_addresses).await?;

    info!(
        chunks_fetched = result.chunks_fetched,
        chunks_skipped = result.chunks_skipped,
        total_bytes = result.total_bytes,
        merkle_verified = result.merkle_verified,
        peers_contacted = result.peers_contacted.len(),
        duration_ms = result.duration().as_millis() as u64,
        output = %output.display(),
        "download complete"
    );

    net.shutdown().await?;
    Ok(())
}

// ── Send mode ─────────────────────────────────────────────────────────────────

async fn run_send(keypair: Keypair, net_config: NetConfig, message: String) -> Result<()> {
    let node = SumNet::new(net_config, keypair).await?;

    const TIMEOUT: Duration = Duration::from_secs(30);
    info!("waiting for a peer on the LAN (timeout: {TIMEOUT:?})");

    let result =
        tokio::time::timeout(TIMEOUT, discover_and_send(&node, &message)).await;

    match result {
        Ok(inner)     => inner,
        Err(_elapsed) => {
            warn!("timed out — are both nodes on the same LAN?");
            Err(anyhow::anyhow!("no peer discovered within timeout"))
        }
    }
}

async fn discover_and_send(node: &SumNet, message: &str) -> Result<()> {
    while let Some(event) = node.next_event().await {
        print_event(&event);

        if let SumNetEvent::PeerDiscovered { peer_id, .. } = &event {
            info!(%peer_id, "peer found — publishing");
            tokio::time::sleep(Duration::from_millis(500)).await;
            node.publish(TOPIC_TEST, message.as_bytes().to_vec()).await?;
            info!("sent on '{TOPIC_TEST}' — exiting");
            tokio::time::sleep(Duration::from_millis(300)).await;
            node.shutdown().await?;
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("event stream closed before a peer was discovered"))
}

// ── Event printer ─────────────────────────────────────────────────────────────

fn print_event(event: &SumNetEvent) {
    match event {
        SumNetEvent::Listening { addr } =>
            info!("LISTENING    {addr}"),
        SumNetEvent::PeerDiscovered { peer_id, addrs } =>
            info!("DISCOVERED   {peer_id}  addrs={addrs:?}"),
        SumNetEvent::PeerExpired { peer_id } =>
            info!("EXPIRED      {peer_id}"),
        SumNetEvent::PeerConnected { peer_id } =>
            info!("CONNECTED    {peer_id}"),
        SumNetEvent::PeerDisconnected { peer_id } =>
            info!("DISCONNECTED {peer_id}"),
        SumNetEvent::MessageReceived { from, topic, data } => {
            let _text = String::from_utf8_lossy(data);
            info!("MESSAGE      topic={topic}  from={from}  len={}", data.len());
        }
        SumNetEvent::ShardRequested { peer_id, request, channel_id } =>
            info!("CHUNK_REQ    peer={peer_id}  cid={}  ch={channel_id}", request.cid),
        SumNetEvent::ShardReceived { peer_id, response } =>
            info!("CHUNK_RECV   peer={peer_id}  cid={}  offset={}  bytes={}", response.cid, response.offset, response.data.len()),
        SumNetEvent::ShardRequestedV2 { peer_id, request, channel_id } => {
            let kind = match request {
                sum_net::ShardRequestV2::Pull { .. } => "Pull",
                sum_net::ShardRequestV2::Push { .. } => "Push",
                sum_net::ShardRequestV2::ManifestPush { .. } => "ManifestPush",
                sum_net::ShardRequestV2::ManifestPull { .. } => "ManifestPull",
            };
            info!("V2_REQ       peer={peer_id}  kind={kind}  ch={channel_id}");
        }
        SumNetEvent::ShardReceivedV2 { peer_id, response } => {
            let kind = match response {
                sum_net::ShardResponseV2::Data { .. } => "Data",
                sum_net::ShardResponseV2::PushAck { .. } => "PushAck",
                sum_net::ShardResponseV2::ManifestPushAck { .. } => "ManifestPushAck",
                sum_net::ShardResponseV2::ManifestData { .. } => "ManifestData",
            };
            info!("V2_RECV      peer={peer_id}  kind={kind}");
        }
        SumNetEvent::ShardRequestFailed { peer_id, error } =>
            info!("CHUNK_FAIL   peer={peer_id}  error={error}"),
        SumNetEvent::PeerIdentified { peer_id, l1_address } =>
            info!("IDENTIFIED   peer={peer_id}  l1={}", identity::l1_address_base58(l1_address)),
        SumNetEvent::NatStatusChanged { is_public, public_addr } => {
            let kind = if *is_public { "PUBLIC" } else { "PRIVATE" };
            match public_addr {
                Some(addr) => info!("NAT_STATUS   {kind}  addr={addr}"),
                None       => info!("NAT_STATUS   {kind}"),
            }
        }
        SumNetEvent::RelayReservation { relay_peer_id, relay_addr } =>
            info!("RELAY_RSV    relay={relay_peer_id}  addr={relay_addr}"),
        SumNetEvent::HolePunchSucceeded { peer_id } =>
            info!("HOLEPUNCH_OK peer={peer_id}"),
        SumNetEvent::HolePunchFailed { peer_id, error } =>
            info!("HOLEPUNCH_NG peer={peer_id}  error={error}"),
    }
}
