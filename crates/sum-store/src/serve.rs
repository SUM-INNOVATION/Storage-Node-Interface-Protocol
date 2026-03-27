//! Inbound chunk and manifest request handler.
//!
//! When a remote peer requests a chunk (or sub-range), this module reads the
//! chunk from the local [`ChunkStore`] via mmap, slices the requested byte
//! window, and sends the response through [`SumNet::respond_shard`].
//!
//! Manifest requests use the convention: if `request.cid` starts with
//! `"manifest:"`, the remainder is a hex-encoded merkle root. The response
//! contains the CBOR-serialized `DataManifest`.

use sum_net::{SumNet, ShardRequest, ShardResponse};
use tracing::{info, warn};

use crate::manifest;
use crate::manifest_index::ManifestIndex;
use crate::store::ChunkStore;

/// The prefix that distinguishes manifest requests from chunk requests.
pub const MANIFEST_REQUEST_PREFIX: &str = "manifest:";

/// Handle an inbound request (chunk or manifest).
pub async fn handle_request(
    net: &SumNet,
    store: &ChunkStore,
    manifest_idx: &ManifestIndex,
    request: &ShardRequest,
    channel_id: u64,
) {
    if request.cid.starts_with(MANIFEST_REQUEST_PREFIX) {
        handle_manifest_request(net, manifest_idx, request, channel_id).await;
    } else {
        handle_chunk_request(net, store, request, channel_id).await;
    }
}

/// Handle a manifest request: `cid` = `"manifest:<hex_merkle_root>"`.
async fn handle_manifest_request(
    net: &SumNet,
    manifest_idx: &ManifestIndex,
    request: &ShardRequest,
    channel_id: u64,
) {
    let root_hex = &request.cid[MANIFEST_REQUEST_PREFIX.len()..];

    // Parse hex merkle root to [u8; 32]
    let root_bytes = match hex_to_32(root_hex) {
        Some(b) => b,
        None => {
            let resp = ShardResponse {
                cid: request.cid.clone(),
                offset: 0,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(format!("invalid manifest root hex: {root_hex}")),
            };
            let _ = net.respond_shard(channel_id, resp).await;
            return;
        }
    };

    // Look up manifest
    let Some(manifest_data) = manifest_idx.get_by_merkle_root(&root_bytes) else {
        let resp = ShardResponse {
            cid: request.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!("manifest not found for root: {root_hex}")),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    };

    // CBOR-serialize the manifest
    let mut cbor_buf = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(manifest_data, &mut cbor_buf) {
        let resp = ShardResponse {
            cid: request.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!("manifest serialization error: {e}")),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    }

    info!(
        root = root_hex,
        bytes = cbor_buf.len(),
        channel_id,
        "serving manifest"
    );

    let resp = ShardResponse {
        cid: request.cid.clone(),
        offset: 0,
        total_bytes: cbor_buf.len() as u64,
        data: cbor_buf,
        error: None,
    };

    if let Err(e) = net.respond_shard(channel_id, resp).await {
        warn!(root = root_hex, %e, "failed to send manifest response");
    }
}

/// Handle a standard chunk request by CID.
async fn handle_chunk_request(
    net: &SumNet,
    store: &ChunkStore,
    request: &ShardRequest,
    channel_id: u64,
) {
    let cid = &request.cid;

    if !store.has(cid) {
        warn!(%cid, channel_id, "requested chunk not found locally");
        let resp = ShardResponse {
            cid: cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!("chunk not found: {cid}")),
        };
        if let Err(e) = net.respond_shard(channel_id, resp).await {
            warn!(%cid, %e, "failed to send error response");
        }
        return;
    }

    let mapped = match store.mmap(cid) {
        Ok(m) => m,
        Err(e) => {
            warn!(%cid, %e, "failed to mmap chunk");
            let resp = ShardResponse {
                cid: cid.clone(),
                offset: 0,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(format!("mmap error: {e}")),
            };
            let _ = net.respond_shard(channel_id, resp).await;
            return;
        }
    };

    let total = mapped.len() as u64;
    let offset = request.offset.unwrap_or(0).min(total);
    let max_bytes = request.max_bytes.unwrap_or(total);
    let end = (offset + max_bytes).min(total);
    let data = &mapped[offset as usize..end as usize];

    info!(
        %cid,
        offset,
        data_len = data.len(),
        total,
        channel_id,
        "serving chunk data"
    );

    let resp = ShardResponse {
        cid: cid.clone(),
        offset,
        total_bytes: total,
        data: data.to_vec(),
        error: None,
    };

    if let Err(e) = net.respond_shard(channel_id, resp).await {
        warn!(%cid, %e, "failed to send chunk response");
    }
}

/// Parse a hex string into [u8; 32]. Returns None if invalid.
fn hex_to_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(bytes)
}
