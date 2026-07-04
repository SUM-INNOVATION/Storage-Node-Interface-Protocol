//! UniFFI surface for embedding the SNIP client in mobile apps.
//!
//! Wraps the existing sum-node client flows (public V2 ingest and
//! download) behind a small async API. The signing seed crosses the FFI
//! per call and is zeroized after use — the Rust side never stores key
//! material.

mod client;
mod download_flow;
mod error;
mod upload_flow;

use std::sync::Arc;
use std::time::Duration;

use zeroize::Zeroize;

pub use client::{
    DownloadReport, FileInfo, HealthReport, ProgressListener, SnipConfig, TransferPhase,
    UploadReceipt,
};
pub use error::SnipError;

use client::SnipClient as Inner;

uniffi::setup_scaffolding!();

/// Crate version, for an end-to-end FFI smoke test from Swift.
#[uniffi::export]
pub fn snip_core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// BLAKE3 merkle root (0x-hex) of a file, computed exactly as ingest
/// does (1 MiB chunks, odd nodes duplicated). Exported so Swift tests
/// can pin golden vectors against the consensus-critical tree shape.
#[uniffi::export]
pub fn compute_merkle_root(file_path: String) -> Result<String, SnipError> {
    let (_mmap, manifest) =
        sum_store::BinaryChunker::chunk_file(std::path::Path::new(&file_path))
            .map_err(SnipError::io)?;
    Ok(format!("0x{}", hex::encode(manifest.merkle_root)))
}

/// Rendezvous assignment for one chunk: the base58 addresses of the
/// archives that must hold it, ordered ascending by (score, address).
/// Mirrors `sum_store::assignment_v2` — consensus-critical.
#[uniffi::export]
pub fn compute_chunk_assignment(
    merkle_root_hex: String,
    chunk_index: u32,
    node_addresses: Vec<String>,
    replication_factor: u32,
) -> Result<Vec<String>, SnipError> {
    let stripped = merkle_root_hex.trim().trim_start_matches("0x");
    let root_bytes = hex::decode(stripped).map_err(|e| SnipError::InvalidConfig {
        msg: format!("bad merkle root hex: {e}"),
    })?;
    let root: [u8; 32] = root_bytes
        .try_into()
        .map_err(|_| SnipError::InvalidConfig {
            msg: "merkle root must be 32 bytes".into(),
        })?;

    let snapshot: Vec<[u8; 20]> = node_addresses
        .iter()
        .map(|addr| {
            sum_net::l1_address_from_base58(addr).map_err(|e| SnipError::InvalidConfig {
                msg: format!("bad node address {addr}: {e}"),
            })
        })
        .collect::<Result<_, _>>()?;

    let assigned =
        sum_store::assignment_v2::assigned_archives(&root, &snapshot, chunk_index, replication_factor);
    Ok(assigned
        .iter()
        .map(sum_net::l1_address_base58)
        .collect())
}

/// Handle for SNIP storage operations. Construct once with the app's
/// configuration; each operation runs its own swarm lifecycle.
#[derive(uniffi::Object)]
pub struct SnipClient {
    inner: Inner,
}

#[uniffi::export(async_runtime = "tokio")]
impl SnipClient {
    #[uniffi::constructor]
    pub fn new(config: SnipConfig) -> Result<Arc<Self>, SnipError> {
        let inner = Inner { config };
        inner.validate()?;
        Ok(Arc::new(Self { inner }))
    }

    /// Upload a file as a Public V2 SNIP object: register on chain, push
    /// chunks to the rendezvous-assigned archives, replicate the
    /// manifest, wait for coverage, and activate. Returns once the file
    /// is Active. `signer_seed` is the caller's 32-byte Ed25519 seed
    /// (the SUM Chain account paying fees and owning the file).
    pub async fn upload_file(
        &self,
        file_path: String,
        signer_seed: Vec<u8>,
        listener: Option<Arc<dyn ProgressListener>>,
    ) -> Result<UploadReceipt, SnipError> {
        let seed = take_seed(signer_seed)?;
        upload_flow::upload_public(&self.inner, &file_path, seed, listener).await
    }

    /// Download a Public V2 file to `output_path`, verifying every chunk
    /// hash and the full merkle root against the chain-recorded identity.
    /// The seed only derives the libp2p identity for this session; public
    /// downloads perform no signing.
    pub async fn download_file(
        &self,
        merkle_root_hex: String,
        output_path: String,
        signer_seed: Vec<u8>,
        listener: Option<Arc<dyn ProgressListener>>,
    ) -> Result<DownloadReport, SnipError> {
        let seed = take_seed(signer_seed)?;
        download_flow::download_public(&self.inner, &merkle_root_hex, &output_path, seed, listener)
            .await
    }

    /// Chain-side metadata for a stored file (no P2P traffic).
    pub async fn file_info(&self, merkle_root_hex: String) -> Result<FileInfo, SnipError> {
        download_flow::fetch_file_info(&self.inner, &merkle_root_hex).await
    }

    /// Reachability probe: RPC height plus a short peer-discovery window
    /// against the configured bootstrap peers.
    pub async fn health_check(&self) -> HealthReport {
        let rpc = sum_node::rpc_client::L1RpcClient::new(self.inner.config.rpc_url.clone());
        let (rpc_reachable, chain_height) = match rpc.chain_get_block_height().await {
            Ok(h) => (true, h.height),
            Err(_) => (false, 0),
        };

        let peers_discovered = match probe_peers(&self.inner).await {
            Ok(count) => count,
            Err(_) => 0,
        };

        HealthReport {
            rpc_reachable,
            chain_height,
            peers_discovered,
        }
    }
}

/// Ephemeral identity probe: bring a swarm up, count identified peers
/// within a short window, tear it down.
async fn probe_peers(inner: &Inner) -> Result<u32, SnipError> {
    inner.validate()?;
    let mut probe_seed = [0u8; 32];
    // Identity for the probe only; no signing happens. Derive from the
    // config so repeated probes reuse one PeerId instead of churning.
    let digest = blake3::hash(inner.config.rpc_url.as_bytes());
    probe_seed.copy_from_slice(digest.as_bytes());
    let keypair = sum_net::keypair_from_seed(&probe_seed).map_err(SnipError::internal)?;
    let net = sum_net::SumNet::new(inner.net_config(), keypair)
        .await
        .map_err(SnipError::internal)?;
    let peers = client::discover_peers(&net, Duration::from_secs(10)).await;
    let count = peers.read().await.len() as u32;
    let _ = net.shutdown().await;
    Ok(count)
}

/// Move a foreign-provided seed into a fixed array and wipe the vector.
fn take_seed(mut seed_vec: Vec<u8>) -> Result<[u8; 32], SnipError> {
    if seed_vec.len() != 32 {
        seed_vec.zeroize();
        return Err(SnipError::InvalidConfig {
            msg: format!("signer seed must be 32 bytes, got {}", seed_vec.len()),
        });
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_vec);
    seed_vec.zeroize();
    Ok(seed)
}
