//! Inbound chunk request handler.
//!
//! When a remote peer requests a chunk (or sub-range), this module reads the
//! chunk from the local [`ChunkStore`] via mmap, slices the requested byte
//! window, and sends the response through [`SumNet::respond_shard`].

use sum_net::{SumNet, ShardRequest, ShardResponse};
use tracing::{info, warn};

use crate::store::ChunkStore;

/// Handle an inbound chunk request.
///
/// - Reads the chunk from `store` via mmap
/// - Slices `[offset .. offset + max_bytes]` (clamped to chunk size)
/// - Sends the response via `net.respond_shard(channel_id, ...)`
pub async fn handle_request(
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
