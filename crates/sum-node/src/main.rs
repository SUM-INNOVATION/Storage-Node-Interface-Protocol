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

use anyhow::Result;
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
use sum_node::download::DownloadOrchestrator;
use sum_node::market_sync::MarketSyncWorker;
use sum_node::peer_state::{apply_peer_event, PeerMapChange};
use sum_node::por_worker::PorWorker;
use sum_node::profile::{log_profile_banner, NodeProfile};
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
        Command::Fetch { cid } => run_fetch(keypair, net_config, cid).await,
        Command::Download { merkle_root, output, max_concurrent, download_timeout_secs } => {
            run_download(keypair, cli.rpc_url.clone(), net_config, merkle_root, output, max_concurrent, download_timeout_secs).await
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
    } else {
        warn!("PoR/MarketSync workers not started — no L1 keypair available");
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
                                .on_chunk_received(net.as_ref(), &store_mut.local, &response)
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
    if let Err(failure) = upload_result.check_success(sum_types::storage::REPLICATION_FACTOR as u32)
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

async fn run_download(
    keypair: Keypair,
    rpc_url: String,
    net_config: NetConfig,
    merkle_root: String,
    output: PathBuf,
    max_concurrent: usize,
    timeout_secs: u64,
) -> Result<()> {
    info!(%merkle_root, output = %output.display(), "starting download");

    let net = Arc::new(SumNet::new(net_config, keypair.clone()).await?);
    let store = Arc::new(RwLock::new(SumStore::new(StoreConfig::default())?));
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
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
