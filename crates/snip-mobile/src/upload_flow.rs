//! Public V2 ingest, ported from the CLI's `run_ingest_v2` glue: build a
//! swarm, discover peers, populate IngestParams from live chain params
//! (no dev fallback — a wallet always talks to a real chain), run the
//! existing IngestPipeline, and map the outcome to FFI types.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sum_net::SumNet;
use sum_node::ingest_v2::{
    IngestOutcome, IngestParams, IngestParamsDefaults, IngestPipeline, MapPeerResolver,
};
use sum_node::rpc_client::L1RpcClient;

use crate::client::{discover_peers, ProgressListener, SnipClient, TransferPhase, UploadReceipt};
use crate::error::SnipError;

pub(crate) async fn upload_public(
    client: &SnipClient,
    file_path: &str,
    seed: [u8; 32],
    listener: Option<Arc<dyn ProgressListener>>,
) -> Result<UploadReceipt, SnipError> {
    client.validate()?;
    let path = Path::new(file_path);
    if !path.is_file() {
        return Err(SnipError::Io {
            msg: format!("no such file: {file_path}"),
        });
    }

    let phase = |p: TransferPhase| {
        if let Some(l) = &listener {
            l.on_phase(p);
        }
    };

    let keypair = sum_net::keypair_from_seed(&seed).map_err(SnipError::internal)?;
    let l1_addr = sum_net::l1_address_from_keypair(&keypair);
    let rpc = Arc::new(L1RpcClient::new(client.config.rpc_url.clone()));

    // Chain params first: fail before any libp2p work if the chain is
    // unreachable or V2 is disabled (production-profile semantics).
    let chain_params = rpc
        .chain_get_chain_params()
        .await
        .map_err(SnipError::rpc)?;
    if chain_params.v2_enabled_from_height.is_none() {
        return Err(SnipError::V2NotEnabled);
    }

    let defaults = IngestParamsDefaults {
        poll_interval: Duration::from_secs(2),
        activation_wait_secs: Duration::from_secs(client.config.activation_wait_secs),
        finality_timeout: Duration::from_secs(60),
        push_retries: 2,
        push_wait_secs: Duration::from_secs(client.config.push_wait_secs),
        manifest_push_wait_secs: Duration::from_secs(client.config.manifest_push_wait_secs),
    };
    let mut params = IngestParams::from_chain_params(&chain_params, defaults);
    params.fee_per_tx = client.config.fee_per_tx as u128;

    phase(TransferPhase::Connecting);
    let net = Arc::new(
        SumNet::new(client.net_config(), keypair)
            .await
            .map_err(SnipError::internal)?,
    );

    phase(TransferPhase::DiscoveringPeers);
    let peer_addresses = discover_peers(
        &net,
        Duration::from_secs(client.config.discover_timeout_secs),
    )
    .await;
    if peer_addresses.read().await.is_empty() {
        let _ = net.shutdown().await;
        return Err(SnipError::NoPeersReachable);
    }

    phase(TransferPhase::Uploading);
    let resolver = Arc::new(MapPeerResolver::new(peer_addresses));
    let pipeline = IngestPipeline::new(rpc, net.clone(), resolver, seed, l1_addr, params);
    let outcome = pipeline.run(path).await;
    let _ = net.shutdown().await;

    match outcome {
        IngestOutcome::Activated {
            merkle_root,
            manifest,
            register_tx_hash,
            activate_tx_hash,
            ..
        } => {
            phase(TransferPhase::Complete);
            Ok(UploadReceipt {
                merkle_root_hex: format!("0x{}", hex::encode(merkle_root)),
                file_size_bytes: manifest.total_size_bytes,
                chunk_count: manifest.chunk_count,
                register_tx_hash,
                activate_tx_hash,
            })
        }
        IngestOutcome::ActivatedOnChain { merkle_root, manifest, .. } => {
            // Resume-only shape, but harmless to map: the file is Active.
            phase(TransferPhase::Complete);
            Ok(UploadReceipt {
                merkle_root_hex: format!("0x{}", hex::encode(merkle_root)),
                file_size_bytes: manifest.total_size_bytes,
                chunk_count: manifest.chunk_count,
                register_tx_hash: String::new(),
                activate_tx_hash: String::new(),
            })
        }
        IngestOutcome::PendingNeedsAction {
            under_replicated_chunks,
            suggested,
            source,
            ..
        } => Err(SnipError::PushIncomplete {
            chunks_missing: under_replicated_chunks.map(|c| c.len()).unwrap_or(0) as u32,
            detail: format!(
                "{suggested:?}: {}",
                source.map(|e| e.to_string()).unwrap_or_default()
            ),
        }),
        IngestOutcome::Failed { stage, source, .. } => {
            Err(map_failed_stage(format!("{stage:?}"), source.to_string()))
        }
        other => Err(SnipError::Internal {
            msg: format!("unexpected ingest outcome: {other:?}"),
        }),
    }
}

fn map_failed_stage(stage: String, source: String) -> SnipError {
    let lower = source.to_lowercase();
    if lower.contains("insufficient") || lower.contains("balance") {
        return SnipError::TxRejected { reason: source };
    }
    if lower.contains("finality") || lower.contains("timed out waiting for tx") {
        return SnipError::TxTimeout;
    }
    if stage.contains("Register") || stage.contains("Activate") {
        return SnipError::TxRejected {
            reason: format!("{stage}: {source}"),
        };
    }
    if stage.contains("Coverage") {
        return SnipError::CoverageTimeout;
    }
    SnipError::Internal {
        msg: format!("{stage}: {source}"),
    }
}
