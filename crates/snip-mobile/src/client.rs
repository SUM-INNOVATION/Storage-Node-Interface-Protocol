//! SnipClient: configuration, per-operation swarm lifecycle, and the
//! peer-discovery loop shared by upload and health checks.
//!
//! Each operation builds its own SumNet (swarm + background task) and
//! shuts it down when done, mirroring the CLI's `--client` semantics.
//! Mobile apps get suspended and lose sockets at arbitrary points; a
//! fresh swarm per operation is more robust than a long-lived one and
//! costs only connection setup on networks where archives are WAN
//! peers anyway.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sum_net::{PeerId, SumNet, SumNetEvent};
use sum_node::peer_state::apply_peer_event;
use sum_types::config::NetConfig;
use tokio::sync::RwLock;

use crate::error::SnipError;

/// Client configuration. One per SnipClient; all operations inherit it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SnipConfig {
    /// SUM Chain L1 JSON-RPC endpoint, e.g. "https://rpc.sumchain.io".
    pub rpc_url: String,
    /// Kademlia bootstrap peer multiaddrs
    /// (e.g. "/ip4/1.2.3.4/tcp/4001/p2p/12D3KooW…"). Chain node records
    /// carry no network addresses, so WAN discovery starts from these.
    pub bootstrap_peers: Vec<String>,
    /// Directory for the temporary chunk store (app cache dir on iOS).
    pub storage_dir: String,
    /// Enable mDNS LAN discovery. Off for iOS (requires the local-network
    /// entitlement and multicast permission); useful for desktop dev.
    #[uniffi(default = false)]
    pub enable_mdns: bool,
    /// Seconds to wait for peer discovery before an upload gives up.
    #[uniffi(default = 30)]
    pub discover_timeout_secs: u64,
    /// Per-(chunk, archive) push wall-clock budget, seconds.
    #[uniffi(default = 120)]
    pub push_wait_secs: u64,
    /// Manifest replication wall-clock budget, seconds.
    #[uniffi(default = 60)]
    pub manifest_push_wait_secs: u64,
    /// Coverage-polling budget before ActivateFileV2, seconds.
    #[uniffi(default = 300)]
    pub activation_wait_secs: u64,
    /// Download wall-clock budget, seconds.
    #[uniffi(default = 300)]
    pub download_timeout_secs: u64,
    /// Max concurrent chunk pulls per download.
    #[uniffi(default = 10)]
    pub max_concurrent_pulls: u32,
    /// Fee offered per storage transaction, in base units (Koppa).
    #[uniffi(default = 100)]
    pub fee_per_tx: u64,
}

/// Coarse transfer lifecycle for progress UI. Per-chunk granularity needs
/// upstream pipeline hooks and is a planned follow-up; these phases are
/// what the FFI layer itself can observe today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferPhase {
    Connecting,
    DiscoveringPeers,
    Uploading,
    FetchingInfo,
    Downloading,
    Complete,
}

/// Foreign-implemented progress observer (a Swift class in the wallet).
#[uniffi::export(with_foreign)]
pub trait ProgressListener: Send + Sync {
    fn on_phase(&self, phase: TransferPhase);
}

/// Outcome of a successful public upload.
#[derive(Debug, Clone, uniffi::Record)]
pub struct UploadReceipt {
    /// 0x-prefixed 64-hex BLAKE3 merkle root — the file's permanent address.
    pub merkle_root_hex: String,
    pub file_size_bytes: u64,
    pub chunk_count: u32,
    pub register_tx_hash: String,
    pub activate_tx_hash: String,
}

/// Outcome of a successful download.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DownloadReport {
    pub total_bytes: u64,
    pub chunks_fetched: u32,
    pub merkle_verified: bool,
}

/// Chain-side file metadata, for status displays.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FileInfo {
    pub merkle_root_hex: String,
    pub owner: String,
    pub plaintext_size_bytes: u64,
    pub chunk_count: u32,
    pub lifecycle: String,
    pub is_private: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HealthReport {
    pub rpc_reachable: bool,
    pub chain_height: u64,
    pub peers_discovered: u32,
}

pub struct SnipClient {
    pub(crate) config: SnipConfig,
}

impl SnipClient {
    pub(crate) fn net_config(&self) -> NetConfig {
        NetConfig {
            udp_listen_port: 0,
            tcp_listen_port: 0,
            // WAN mode unless the caller is doing LAN-only development:
            // bootstrap peers are the only route to archives from a phone.
            enable_wan: !self.config.bootstrap_peers.is_empty(),
            bootstrap_peers: self.config.bootstrap_peers.clone(),
            relay_server: false,
            client_mode: true,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), SnipError> {
        if self.config.rpc_url.is_empty() {
            return Err(SnipError::InvalidConfig {
                msg: "rpc_url is empty".into(),
            });
        }
        if self.config.storage_dir.is_empty() {
            return Err(SnipError::InvalidConfig {
                msg: "storage_dir is empty".into(),
            });
        }
        if self.config.bootstrap_peers.is_empty() && !self.config.enable_mdns {
            return Err(SnipError::InvalidConfig {
                msg: "no bootstrap peers configured and mDNS disabled — no way to reach archives"
                    .into(),
            });
        }
        Ok(())
    }
}

/// Drain swarm events until at least one peer is identified (address
/// mapping known) or the deadline passes. Same shape as the CLI's
/// discovery loop in `run_ingest_v2`, minus stdout printing.
pub(crate) async fn discover_peers(
    net: &SumNet,
    timeout: Duration,
) -> Arc<RwLock<HashMap<PeerId, [u8; 20]>>> {
    let peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut found_peer = false;

    loop {
        tokio::select! {
            Some(event) = net.next_event() => {
                match &event {
                    SumNetEvent::PeerDiscovered { .. } => {
                        found_peer = true;
                    }
                    _ => {
                        let mut map = peer_addresses.write().await;
                        let _ = apply_peer_event(&mut map, &event);
                    }
                }
                let map_empty = peer_addresses.read().await.is_empty();
                if found_peer && map_empty {
                    continue;
                }
                if found_peer && !map_empty {
                    // Give stragglers a beat to identify, then settle.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while let Ok(Some(event)) = tokio::time::timeout(
                        Duration::from_millis(200), net.next_event()
                    ).await {
                        let mut map = peer_addresses.write().await;
                        let _ = apply_peer_event(&mut map, &event);
                    }
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    peer_addresses
}
