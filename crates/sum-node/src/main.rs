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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use sum_net::{SumNet, SumNetEvent, Keypair, ShardResponse, TOPIC_STORAGE, TOPIC_TEST};
use sum_net::identity;
use sum_store::{SumStore, FetchOutcome, decode_announcement};
use sum_types::config::{NetConfig, StoreConfig};

use sum_node::acl::AclChecker;
use sum_node::market_sync::MarketSyncWorker;
use sum_node::por_worker::PorWorker;
use sum_node::rpc_client::L1RpcClient;

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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen indefinitely, serving chunk requests, enforcing ACLs,
    /// and responding to PoR challenges.
    Listen,

    /// Ingest a file: chunk, build Merkle tree, store, announce on mesh.
    Ingest {
        /// Path to the file to ingest.
        path: PathBuf,
    },

    /// Fetch a chunk by CID from a LAN peer.
    Fetch {
        /// CIDv1 string of the chunk to fetch.
        cid: String,
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

    match cli.command {
        Command::Listen => run_listen(keypair, seed, &cli).await,
        Command::Ingest { path } => run_ingest(keypair, path).await,
        Command::Fetch { cid } => run_fetch(keypair, cid).await,
        Command::Send { message } => run_send(keypair, message).await,
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

async fn run_listen(keypair: Keypair, seed: Option<[u8; 32]>, cli: &Cli) -> Result<()> {
    let net = Arc::new(SumNet::new(NetConfig::default(), keypair.clone()).await?);
    let store = Arc::new(RwLock::new(SumStore::new(StoreConfig::default())?));

    // Shared PeerId -> L1 Address map (populated by PeerIdentified events).
    let peer_addresses: Arc<RwLock<HashMap<sum_net::PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // RPC client.
    let rpc = Arc::new(L1RpcClient::new(cli.rpc_url.clone()));
    info!(rpc_url = %cli.rpc_url, "L1 RPC client initialized");

    // ACL checker.
    let acl = AclChecker::new(rpc.clone(), peer_addresses.clone());

    // Spawn PoR worker if we have an L1 keypair.
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
        tokio::spawn(async move { por.run(store_clone).await });
        // Spawn MarketSync worker
        let market_sync = MarketSyncWorker::new(
            rpc.clone(),
            l1_addr,
            l1_base58.clone(),
            Duration::from_secs(cli.market_sync_secs),
        );
        let store_clone2 = store.clone();
        let net_clone = net.clone();
        let peer_addrs_clone = peer_addresses.clone();
        tokio::spawn(async move {
            market_sync.run(store_clone2, net_clone, peer_addrs_clone).await;
        });
    } else {
        warn!("PoR/MarketSync workers not started — no L1 keypair available");
    }

    info!("SUM Node listening — serving chunks, enforcing ACLs, press Ctrl-C to stop");

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::ShardRequested { peer_id, request, channel_id } => {
                        // ACL check before serving.
                        let store_read = store.read().await;
                        let allowed = acl
                            .check_access(peer_id, &request.cid, &store_read.manifest_idx)
                            .await
                            .unwrap_or_else(|e| {
                                warn!(%e, "ACL check failed (RPC error) — denying");
                                false
                            });

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
                    SumNetEvent::PeerIdentified { peer_id, l1_address } => {
                        peer_addresses.write().await.insert(*peer_id, *l1_address);
                        info!(
                            peer = %peer_id,
                            l1 = %identity::l1_address_base58(l1_address),
                            "peer L1 identity mapped"
                        );
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

// ── Ingest mode ──────────────────────────────────────────────────────────────

async fn run_ingest(keypair: Keypair, path: PathBuf) -> Result<()> {
    let net = SumNet::new(NetConfig::default(), keypair).await?;
    let mut store = SumStore::new(StoreConfig::default())?;

    info!(path = %path.display(), "ingesting file");
    let manifest = store.ingest_file(&path)?;

    let json = serde_json::to_string_pretty(&manifest)?;
    info!("manifest:\n{json}");

    // Wait for a peer before announcing.
    info!("waiting for a peer on the LAN to announce chunks...");
    let timeout = Duration::from_secs(30);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                print_event(&event);
                if let SumNetEvent::PeerDiscovered { peer_id, .. } = &event {
                    info!(%peer_id, "peer found — announcing chunks");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    store.announce_chunks(&net, &manifest).await?;
                    info!("all chunks announced — listening for requests");
                    simple_serve_loop(&net, &store).await?;
                    return Ok(());
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                warn!("timed out waiting for peer — announcing anyway");
                store.announce_chunks(&net, &manifest).await?;
                simple_serve_loop(&net, &store).await?;
                return Ok(());
            }
        }
    }
}

/// Simple serve loop without ACL (used by ingest after announcing).
async fn simple_serve_loop(net: &SumNet, store: &SumStore) -> Result<()> {
    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::ShardRequested { request, channel_id, .. } => {
                        sum_store::serve::handle_request(
                            net, &store.local, &store.manifest_idx,
                            request, *channel_id,
                        ).await;
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

// ── Fetch mode ───────────────────────────────────────────────────────────────

async fn run_fetch(keypair: Keypair, cid: String) -> Result<()> {
    let net = SumNet::new(NetConfig::default(), keypair).await?;
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

// ── Send mode ─────────────────────────────────────────────────────────────────

async fn run_send(keypair: Keypair, message: String) -> Result<()> {
    let node = SumNet::new(NetConfig::default(), keypair).await?;

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
        SumNetEvent::ShardRequestFailed { peer_id, error } =>
            info!("CHUNK_FAIL   peer={peer_id}  error={error}"),
        SumNetEvent::PeerIdentified { peer_id, l1_address } =>
            info!("IDENTIFIED   peer={peer_id}  l1={}", identity::l1_address_base58(l1_address)),
    }
}
