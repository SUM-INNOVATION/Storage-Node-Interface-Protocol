//! Shared V2 routing helpers for the Public and Private download paths.
//!
//! Both `download::DownloadOrchestrator::run_v2_public` and
//! `download_private::run_download_private` need the same chain-deterministic
//! per-chunk archive list to satisfy V2's assignment rule. The construction is
//! pure given a chain snapshot + replication factor, so it lives here as a
//! reusable helper rather than being duplicated inline.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use thiserror::Error;

use sum_store::manifest::deserialize_manifest_cbor;
use sum_types::rpc_types::StorageFileInfoV2;
use sum_types::storage::DataManifest;

use crate::rpc_client::L1RpcClient;

// ── V2 assignment view ──────────────────────────────────────────────────────

/// Chain-deterministic V2 routing view: the active-archive snapshot at
/// `info.assignment_height`, the chain's replication factor, and the
/// resulting per-chunk archive list.
///
/// Both fields are sorted ascending by L1 address so consumers can do
/// stable iteration. `distinct_assigned` is the union over all chunks
/// — used by manifest fan-out where any one assigned archive is
/// sufficient. `per_chunk_assigned` is keyed by `chunk_index` and
/// holds the V2-deterministic ordered list (score asc, address asc)
/// for routing chunk pulls.
pub struct V2AssignmentView {
    /// Sorted active-archive snapshot at `info.assignment_height`.
    pub snapshot: Vec<[u8; 20]>,
    /// `chain_getChainParams.assignment_replication_factor`.
    pub r: u32,
    /// Union of every chunk's assigned-archive set; deduped by address.
    pub distinct_assigned: BTreeSet<[u8; 20]>,
    /// Per-chunk assigned archives, in V2-deterministic order.
    pub per_chunk_assigned: HashMap<u32, Vec<[u8; 20]>>,
}

/// Errors `build_v2_assignment_view` surfaces. Each variant pins a
/// distinct upstream failure so callers can present an actionable
/// message; collapsing to `anyhow` would erase the snapshot-empty
/// vs. RPC-failed distinction that matters for operator debugging.
#[derive(Debug, Error)]
pub enum V2AssignmentError {
    #[error("storage_getActiveNodesAtHeight({height}): {source}")]
    Snapshot {
        height: u64,
        #[source]
        source: anyhow::Error,
    },
    #[error("snapshot l1 address parse for entry {addr_b58:?}: {source}")]
    SnapshotAddrParse {
        addr_b58: String,
        #[source]
        source: anyhow::Error,
    },
    #[error(
        "snapshot at assignment_height={height} has no active archives — \
         cannot route V2 requests"
    )]
    SnapshotEmpty { height: u64 },
    #[error("chain_getChainParams: {0}")]
    ChainParams(#[source] anyhow::Error),
    #[error(
        "chunk {chunk_index} has no V2-assigned archives (snapshot has {snapshot_len} entries, R={r}) — \
         chain plan §5.3 invariant violation"
    )]
    EmptyAssignedSet {
        chunk_index: u32,
        snapshot_len: usize,
        r: u32,
    },
}

/// Build the V2 assignment view for `info` against `chunk_indices`.
///
/// `chunk_indices` is the iterator over `chunk_index` values to compute
/// per-chunk assignments for. Public V2 typically passes
/// `0..info.chunk_count`; Private's manifest fan-out only needs the
/// `distinct_assigned` set, but the per-chunk map is cheap to build
/// and useful for the chunk-fetch step that follows it.
///
/// Pure modulo two RPC reads (`storage_getActiveNodesAtHeight` +
/// `chain_getChainParams`); no networking, no libp2p state, no
/// peer-id resolution. Resolving L1-addr → PeerId happens later in
/// the orchestrator's per-chunk dispatcher because it depends on
/// runtime peer discovery.
pub async fn build_v2_assignment_view(
    rpc: &L1RpcClient,
    info: &StorageFileInfoV2,
    merkle_root: [u8; 32],
    chunk_indices: impl IntoIterator<Item = u32>,
) -> std::result::Result<V2AssignmentView, V2AssignmentError> {
    let snapshot_records = rpc
        .storage_get_active_nodes_at_height(info.assignment_height)
        .await
        .map_err(|e| V2AssignmentError::Snapshot {
            height: info.assignment_height,
            source: e,
        })?;
    // Apply the shared eligibility contract before address decode so
    // Slashed/Unbonding/Withdrawn/unknown-future and Validator rows
    // are excluded from the V2 assignment view.
    let snapshot_records = sum_types::rpc_types::filter_active_archives(snapshot_records);
    let mut snapshot: Vec<[u8; 20]> = Vec::with_capacity(snapshot_records.len());
    for n in &snapshot_records {
        let addr = sum_net::identity::l1_address_from_base58(&n.address).map_err(|e| {
            V2AssignmentError::SnapshotAddrParse {
                addr_b58: n.address.clone(),
                source: anyhow::anyhow!(e),
            }
        })?;
        snapshot.push(addr);
    }
    snapshot.sort();
    if snapshot.is_empty() {
        return Err(V2AssignmentError::SnapshotEmpty {
            height: info.assignment_height,
        });
    }
    let chain_params = rpc
        .chain_get_chain_params()
        .await
        .map_err(V2AssignmentError::ChainParams)?;
    let r = chain_params.assignment_replication_factor;

    let mut distinct_assigned: BTreeSet<[u8; 20]> = BTreeSet::new();
    let mut per_chunk_assigned: HashMap<u32, Vec<[u8; 20]>> = HashMap::new();
    for chunk_index in chunk_indices {
        let assigned =
            sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, chunk_index, r);
        if assigned.is_empty() {
            return Err(V2AssignmentError::EmptyAssignedSet {
                chunk_index,
                snapshot_len: snapshot.len(),
                r,
            });
        }
        for addr in &assigned {
            distinct_assigned.insert(*addr);
        }
        per_chunk_assigned.insert(chunk_index, assigned);
    }

    Ok(V2AssignmentView {
        snapshot,
        r,
        distinct_assigned,
        per_chunk_assigned,
    })
}

// ── V2 manifest decode ──────────────────────────────────────────────────────

/// Errors `decode_v2_manifest_bytes` surfaces. Distinct from the
/// generic `anyhow!("CBOR error")` form so callers can pin which
/// rejection happened in tests + diagnostics.
#[derive(Debug, Error)]
pub enum ManifestDecodeError {
    #[error("manifest CBOR deserialize failed: {0}")]
    Cbor(String),
    #[error("manifest merkle_root mismatch: peer returned {got} but caller expected {want}")]
    RootMismatch { got: String, want: String },
}

/// Verify that `manifest_bytes` (CBOR `DataManifest`) parses cleanly
/// and that its embedded `merkle_root` equals the chain-side
/// `expected_root` we asked for. Both checks need to pass before any
/// downstream use — a peer that returns a manifest for a different
/// root would otherwise pass merkle/chunk verification on its own
/// data, and the orchestrator would happily download the wrong file.
///
/// This wraps `sum_store::manifest::deserialize_manifest_cbor` plus a
/// 32-byte equality check; the explicit error type is the testable
/// boundary that lets us assert on the two distinct rejection paths
/// without round-tripping through an `anyhow` string.
pub fn decode_v2_manifest_bytes(
    manifest_bytes: &[u8],
    expected_root: [u8; 32],
) -> std::result::Result<DataManifest, ManifestDecodeError> {
    let manifest = deserialize_manifest_cbor(manifest_bytes)
        .map_err(|e| ManifestDecodeError::Cbor(e.to_string()))?;
    if manifest.merkle_root != expected_root {
        return Err(ManifestDecodeError::RootMismatch {
            got: hex::encode(manifest.merkle_root),
            want: hex::encode(expected_root),
        });
    }
    Ok(manifest)
}

/// `Result` alias for callers that want to collapse to `anyhow::Error`
/// at the boundary. Keeps the typed error inside helper-internal code
/// while the orchestrator can `?` over it.
pub type V2RoutingResult<T> = Result<T>;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use sum_types::storage::{ChunkDescriptor, DataManifest};

    fn fixture_manifest(root: [u8; 32]) -> DataManifest {
        DataManifest {
            file_name: "fixture.bin".into(),
            file_hash: [0xAB; 32],
            total_size_bytes: 1024,
            chunk_count: 1,
            merkle_root: root,
            chunks: vec![ChunkDescriptor {
                chunk_index: 0,
                offset: 0,
                size: 1024,
                blake3_hash: [0xCC; 32],
                cid: format!("cid:{}", hex::encode(root)),
                plaintext_blake3_hash: None,
            }],
        }
    }

    /// Match the CBOR encoder used by `sum_store::manifest::write_manifest`
    /// so our test bytes are deserialized through the exact same
    /// production path the helper exercises.
    fn cbor_encode_manifest(m: &DataManifest) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(m, &mut buf).expect("CBOR encode");
        buf
    }

    #[test]
    fn decode_v2_manifest_round_trips_when_root_matches() {
        let root = [0x42u8; 32];
        let m = fixture_manifest(root);
        let bytes = cbor_encode_manifest(&m);
        let got = decode_v2_manifest_bytes(&bytes, root).expect("decode");
        assert_eq!(got.merkle_root, root);
        assert_eq!(got.chunk_count, 1);
        assert_eq!(got.chunks[0].chunk_index, 0);
    }

    #[test]
    fn decode_v2_manifest_rejects_root_mismatch() {
        // Peer returns a manifest for a different file. We MUST reject
        // — otherwise the orchestrator would download bytes that
        // verify against THEIR merkle root, not the one the operator
        // asked for. Failing closed here is a privacy + correctness
        // load-bearing check.
        let asked = [0x42u8; 32];
        let returned = [0xAAu8; 32];
        let m = fixture_manifest(returned);
        let bytes = cbor_encode_manifest(&m);
        let err = decode_v2_manifest_bytes(&bytes, asked).expect_err("must reject");
        match err {
            ManifestDecodeError::RootMismatch { got, want } => {
                assert_eq!(got, hex::encode(returned));
                assert_eq!(want, hex::encode(asked));
            }
            other => panic!("expected RootMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_v2_manifest_rejects_corrupt_cbor() {
        // Random bytes are not valid CBOR. The error must be a `Cbor`
        // variant — collapsing to a generic anyhow! would erase the
        // "is this a peer-side mistake or our own caller bug" signal.
        let asked = [0x11u8; 32];
        let err = decode_v2_manifest_bytes(b"\xff\xff not cbor \x00\x00", asked)
            .expect_err("must reject");
        match err {
            ManifestDecodeError::Cbor(_) => {}
            other => panic!("expected Cbor, got {other:?}"),
        }
    }
}
