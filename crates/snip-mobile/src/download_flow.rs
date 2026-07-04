//! Public V2 download, ported from the CLI's `run_download`: probe the
//! chain row first (fail fast before any libp2p work), then run the
//! existing DownloadOrchestrator against a fresh swarm.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sum_net::{PeerId, SumNet};
use sum_node::download::DownloadOrchestrator;
use sum_node::rpc_client::L1RpcClient;
use sum_store::SumStore;
use sum_types::config::StoreConfig;
use tokio::sync::RwLock;

use crate::client::{DownloadReport, FileInfo, ProgressListener, SnipClient, TransferPhase};
use crate::error::SnipError;

/// The RPC returns Option-shaped JSON (null when no V2 row exists), which
/// the typed client surfaces as a deserialize error. Classify that as
/// NotFound; everything else is a real RPC failure.
fn classify_info_error(e: anyhow::Error) -> SnipError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("null") || lower.contains("missing field") || lower.contains("invalid type") {
        SnipError::NotFound
    } else {
        SnipError::Rpc { msg }
    }
}

fn normalize_root_hex(merkle_root_hex: &str) -> Result<String, SnipError> {
    let stripped = merkle_root_hex.trim().trim_start_matches("0x");
    if stripped.len() != 64 || !stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SnipError::InvalidConfig {
            msg: format!("merkle root must be 64 hex chars: {merkle_root_hex}"),
        });
    }
    Ok(format!("0x{}", stripped.to_lowercase()))
}

pub(crate) async fn fetch_file_info(
    client: &SnipClient,
    merkle_root_hex: &str,
) -> Result<FileInfo, SnipError> {
    let root_hex = normalize_root_hex(merkle_root_hex)?;
    let rpc = L1RpcClient::new(client.config.rpc_url.clone());
    let info = rpc
        .storage_get_file_info_v2(&root_hex, None, None)
        .await
        .map_err(classify_info_error)?;

    Ok(FileInfo {
        merkle_root_hex: info.merkle_root.clone(),
        owner: info.owner.clone(),
        plaintext_size_bytes: info.plaintext_size_bytes,
        chunk_count: info.chunk_count,
        lifecycle: format!("{:?}", info.lifecycle),
        is_private: info.visibility.is_private(),
    })
}

pub(crate) async fn download_public(
    client: &SnipClient,
    merkle_root_hex: &str,
    output_path: &str,
    seed: [u8; 32],
    listener: Option<Arc<dyn ProgressListener>>,
) -> Result<DownloadReport, SnipError> {
    client.validate()?;
    let root_hex = normalize_root_hex(merkle_root_hex)?;

    let phase = |p: TransferPhase| {
        if let Some(l) = &listener {
            l.on_phase(p);
        }
    };

    phase(TransferPhase::FetchingInfo);
    let rpc = Arc::new(L1RpcClient::new(client.config.rpc_url.clone()));
    let info = rpc
        .storage_get_file_info_v2(&root_hex, None, None)
        .await
        .map_err(classify_info_error)?;

    if info.visibility.is_private() {
        return Err(SnipError::PrivateUnsupported);
    }

    phase(TransferPhase::Connecting);
    let keypair = sum_net::keypair_from_seed(&seed).map_err(SnipError::internal)?;
    let net = Arc::new(
        SumNet::new(client.net_config(), keypair)
            .await
            .map_err(SnipError::internal)?,
    );

    let store_config = StoreConfig {
        store_dir: PathBuf::from(&client.config.storage_dir),
        ..StoreConfig::default()
    };
    let store = Arc::new(RwLock::new(
        SumStore::new(store_config).map_err(SnipError::io)?,
    ));
    let peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));

    phase(TransferPhase::Downloading);
    let orchestrator = DownloadOrchestrator::new(
        root_hex,
        PathBuf::from(output_path),
        rpc,
        client.config.max_concurrent_pulls as usize,
        Duration::from_secs(client.config.download_timeout_secs),
    );

    let result = orchestrator
        .run_v2_public(net.clone(), store, peer_addresses, info)
        .await;
    let _ = net.shutdown().await;

    let result = result.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_lowercase();
        if lower.contains("no peer") || lower.contains("no assigned") {
            SnipError::NoPeersReachable
        } else if lower.contains("merkle") || lower.contains("hash mismatch") {
            SnipError::IntegrityFailure { msg }
        } else if lower.contains("not active") || lower.contains("pending") {
            SnipError::NotActive { lifecycle: msg }
        } else {
            SnipError::Internal { msg }
        }
    })?;

    if !result.merkle_verified {
        return Err(SnipError::IntegrityFailure {
            msg: "downloaded content does not match the chain-recorded merkle root".into(),
        });
    }

    phase(TransferPhase::Complete);
    Ok(DownloadReport {
        total_bytes: result.total_bytes,
        chunks_fetched: result.chunks_fetched as u32,
        merkle_verified: result.merkle_verified,
    })
}
