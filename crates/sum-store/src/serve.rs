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

use crate::manifest_index::ManifestIndex;
use crate::store::ChunkStore;
use crate::verify;

/// The prefix that distinguishes manifest requests from chunk requests.
pub const MANIFEST_REQUEST_PREFIX: &str = "manifest:";

/// Handle an inbound request that does NOT mutate the manifest index.
///
/// Three cases:
/// * Manifest pull (`cid` starts with `manifest:`, no `push_data`)
/// * Chunk push (`cid` is a chunk CID, `push_data` is `Some`)
/// * Chunk pull (`cid` is a chunk CID, no `push_data`)
///
/// **Manifest pushes** (`cid` starts with `manifest:` AND `push_data` is
/// `Some`) need write access to [`ManifestIndex`] to insert the new
/// manifest, so they MUST be dispatched separately by the caller via
/// [`handle_manifest_push`] under a write lock. This function will
/// reject such a request with an error response, since it can't
/// silently fall through to the manifest-pull branch and risk leaking
/// the wrong response shape.
pub async fn handle_request(
    net: &SumNet,
    store: &ChunkStore,
    manifest_idx: &ManifestIndex,
    request: &ShardRequest,
    channel_id: u64,
) {
    let is_manifest = request.cid.starts_with(MANIFEST_REQUEST_PREFIX);
    let is_push = request.push_data.is_some();
    match (is_manifest, is_push) {
        (true, true) => {
            // Manifest push must be routed via `handle_manifest_push` —
            // tell the caller they hit the wrong dispatch.
            warn!(
                cid = %request.cid,
                channel_id,
                "manifest push routed through read-only handle_request — dropping"
            );
            let resp = ShardResponse {
                cid: request.cid.clone(),
                offset: 0,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(
                    "manifest push must be dispatched via handle_manifest_push (write lock)".into(),
                ),
            };
            let _ = net.respond_shard(channel_id, resp).await;
        }
        (true, false) => handle_manifest_request(net, manifest_idx, request, channel_id).await,
        (false, true) => handle_push_request(net, store, request, channel_id).await,
        (false, false) => handle_chunk_request(net, store, request, channel_id).await,
    }
}

/// Handle an inbound manifest push: deserialize, validate the merkle
/// root matches the CID's hex suffix, persist into [`ManifestIndex`],
/// ACK.
///
/// Idempotent — if the manifest is already indexed, returns ACK without
/// re-inserting.
///
/// # Why this is a separate function
///
/// Inserting into [`ManifestIndex`] needs `&mut`, which requires the
/// caller to hold a write lock. [`handle_request`] takes `&ManifestIndex`
/// because chunk pulls and chunk pushes only need read access. Splitting
/// here keeps the read-lock fast path fast and routes the rare manifest-
/// push case through the write-lock side without forcing every chunk
/// request through one.
pub async fn handle_manifest_push(
    net: &SumNet,
    manifest_idx: &mut ManifestIndex,
    request: &ShardRequest,
    channel_id: u64,
) {
    let Some(data) = request.push_data.as_ref() else {
        // Should not happen — caller is expected to dispatch only when
        // `push_data` is Some. Be defensive anyway.
        let resp = ShardResponse {
            cid: request.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some("handle_manifest_push called without push_data".into()),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    };

    let Some(root_hex) = request.cid.strip_prefix(MANIFEST_REQUEST_PREFIX) else {
        let resp = ShardResponse {
            cid: request.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!(
                "handle_manifest_push: cid does not start with `{MANIFEST_REQUEST_PREFIX}`"
            )),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    };

    let manifest = match validate_manifest_push(root_hex, data.as_slice()) {
        Ok(m) => m,
        Err(reason) => {
            let resp = ShardResponse {
                cid: request.cid.clone(),
                offset: 0,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(reason),
            };
            let _ = net.respond_shard(channel_id, resp).await;
            return;
        }
    };
    let expected_root = manifest.merkle_root;

    // Idempotent insert.
    if manifest_idx.get_by_merkle_root(&expected_root).is_some() {
        info!(root = root_hex, "manifest push: already indexed — ACKing");
    } else if let Err(e) = manifest_idx.insert(&manifest) {
        let resp = ShardResponse {
            cid: request.cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!("manifest_idx.insert failed: {e}")),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    } else {
        info!(
            root = root_hex,
            file_name = %manifest.file_name,
            chunk_count = manifest.chunk_count,
            "manifest pushed — indexed"
        );
    }

    let resp = ShardResponse {
        cid: request.cid.clone(),
        offset: 0,
        total_bytes: data.len() as u64,
        data: Vec::new(),
        error: None,
    };
    if let Err(e) = net.respond_shard(channel_id, resp).await {
        warn!(root = root_hex, %e, "failed to send manifest-push ACK");
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

/// Handle a push (store) request: verify CID, write to disk, ACK.
///
/// The sender is proactively delivering chunk data. We verify the CID
/// matches the data (blake3 hash), store it (idempotent), and respond
/// with an empty-data ACK.
async fn handle_push_request(
    net: &SumNet,
    store: &ChunkStore,
    request: &ShardRequest,
    channel_id: u64,
) {
    let cid = &request.cid;
    let data = request.push_data.as_ref().unwrap();

    // Verify CID matches data
    if let Err(e) = verify::verify_cid(data, cid) {
        warn!(%cid, %e, "push rejected: CID verification failed");
        let resp = ShardResponse {
            cid: cid.clone(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some(format!("CID verification failed: {e}")),
        };
        let _ = net.respond_shard(channel_id, resp).await;
        return;
    }

    // Write to disk (idempotent — skip if already exists)
    if !store.has(cid) {
        if let Err(e) = store.put(cid, data) {
            warn!(%cid, %e, "push rejected: store write failed");
            let resp = ShardResponse {
                cid: cid.clone(),
                offset: 0,
                total_bytes: 0,
                data: Vec::new(),
                error: Some(format!("store write failed: {e}")),
            };
            let _ = net.respond_shard(channel_id, resp).await;
            return;
        }
    }

    info!(%cid, bytes = data.len(), "push accepted — chunk stored");

    // ACK: empty data, no error
    let resp = ShardResponse {
        cid: cid.clone(),
        offset: 0,
        total_bytes: data.len() as u64,
        data: Vec::new(),
        error: None,
    };
    if let Err(e) = net.respond_shard(channel_id, resp).await {
        warn!(%cid, %e, "failed to send push ACK");
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

/// Validate an inbound manifest-push payload and return the
/// deserialized [`DataManifest`] on success. Pure function — no I/O,
/// no network, no manifest-index mutation. Lives separately from
/// [`handle_manifest_push`] so it's straightforward to test.
///
/// Validation enforces full **internal consistency** of the manifest
/// against the encoded merkle root. Without these checks, a malicious
/// peer could push a structurally-valid CBOR blob whose `merkle_root`
/// field matches the CID-encoded root but whose `chunks` list bears no
/// relation to the actual file — poisoning the receiver's `cid → root`
/// index and letting unauthorized CIDs pass production ACL.
///
/// Failure cases (all produce wire-level error responses):
///
/// 1. `root_hex` is not 64 lower-case hex characters.
/// 2. `data` is not valid CBOR or doesn't match the [`DataManifest`]
///    shape.
/// 3. `manifest.merkle_root` ≠ the CID-encoded root.
/// 4. `manifest.chunk_count` ≠ `manifest.chunks.len()`.
/// 5. Any `manifest.chunks[i].chunk_index` ≠ `i` (indices must be
///    contiguous and in order — the merkle leaves are taken in this
///    order, so any reordering breaks the merkle proof).
/// 6. Any chunk's `cid` ≠ the CID derived from its `blake3_hash` (the
///    receiver may not have the chunk bytes here, but every chunk's
///    `cid ↔ blake3_hash` pairing must be self-consistent so the
///    `cid → root` index can never be coerced into pointing at a CID
///    that doesn't correspond to a real merkle leaf).
/// 7. The merkle root recomputed from `chunks[*].blake3_hash` (in
///    order) ≠ `manifest.merkle_root`. This is the structural integrity
///    check that makes 4–6 sufficient: once the recomputed root binds,
///    every chunk descriptor in the list is provably part of the file.
pub fn validate_manifest_push(
    root_hex: &str,
    data: &[u8],
) -> Result<sum_types::storage::DataManifest, String> {
    let expected_root = hex_to_32(root_hex)
        .ok_or_else(|| format!("invalid manifest root hex: {root_hex}"))?;

    let manifest: sum_types::storage::DataManifest = ciborium::de::from_reader(data)
        .map_err(|e| format!("manifest deserialization failed: {e}"))?;

    // (3) merkle_root field matches the CID-encoded root.
    if manifest.merkle_root != expected_root {
        let actual_hex: String = manifest
            .merkle_root
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        return Err(format!(
            "manifest merkle_root mismatch: cid says {root_hex}, manifest says {actual_hex}"
        ));
    }

    // (4) chunk_count agrees with the chunk vector length.
    if manifest.chunks.len() != manifest.chunk_count as usize {
        return Err(format!(
            "manifest chunk_count mismatch: header says {} chunks, vector has {}",
            manifest.chunk_count,
            manifest.chunks.len()
        ));
    }

    // (5) chunk indices form `0..chunk_count` in ascending order.
    // (6) every chunk's `cid` matches its `blake3_hash`.
    // We collect the leaf hashes in the same pass for the recomputation step.
    let mut leaf_hashes: Vec<blake3::Hash> = Vec::with_capacity(manifest.chunks.len());
    for (i, chunk) in manifest.chunks.iter().enumerate() {
        if chunk.chunk_index as usize != i {
            return Err(format!(
                "manifest chunks out of order at position {i}: chunk_index = {}",
                chunk.chunk_index
            ));
        }
        let hash = blake3::Hash::from(chunk.blake3_hash);
        let expected_cid = crate::content_id::cid_from_blake3_hash(&hash);
        if chunk.cid != expected_cid {
            return Err(format!(
                "manifest chunk {i} cid does not match its blake3_hash: \
                 cid={} expected_cid={expected_cid}",
                chunk.cid
            ));
        }
        leaf_hashes.push(hash);
    }

    // (7) Merkle root recomputed from the chunk leaves matches.
    // This is the structural binding: with 5 + 6 holding, the leaf
    // hashes are exactly what the manifest claims, so the root we
    // recompute is the only root those leaves can produce.
    let recomputed_root = *crate::merkle::MerkleTree::build(&leaf_hashes).root().as_bytes();
    if recomputed_root != manifest.merkle_root {
        let computed_hex: String = recomputed_root
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        return Err(format!(
            "manifest merkle root recomputation failed: chunks compute to {computed_hex}, \
             manifest claims {root_hex}"
        ));
    }

    Ok(manifest)
}

/// Parse a hex string into [u8; 32]. Returns None if invalid.
///
/// Strict on encoding: 64 characters AND all-lowercase hex digits.
/// `u8::from_str_radix(_, 16)` would accept uppercase too, but the
/// protocol always emits manifest CIDs via `format!("{b:02x}")` so the
/// receiver enforces the same. Rejecting uppercase up front keeps the
/// `cid → root` lookup canonical (one byte string per file) and avoids
/// any chance of two distinct on-the-wire CID encodings resolving to
/// the same root.
fn hex_to_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    if hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    /// Build a structurally-valid manifest with `n` chunks. `seed` is
    /// XOR'd into the chunk content so callers can request distinct
    /// files (different leaves → different merkle roots) for tests
    /// that compare manifests across files.
    fn well_formed_manifest(n: u32, seed: u8) -> DataManifest {
        let mut chunks = Vec::with_capacity(n as usize);
        let mut leaf_hashes: Vec<blake3::Hash> = Vec::with_capacity(n as usize);
        for i in 0..n {
            let data = [(i as u8) ^ seed; 32];
            let hash = blake3::hash(&data);
            chunks.push(ChunkDescriptor {
                chunk_index: i,
                offset: i as u64 * 32,
                size: 32,
                blake3_hash: *hash.as_bytes(),
                cid: crate::content_id::cid_from_blake3_hash(&hash),
                plaintext_blake3_hash: None,
            });
            leaf_hashes.push(hash);
        }
        let root = *crate::merkle::MerkleTree::build(&leaf_hashes).root().as_bytes();
        DataManifest {
            merkle_root: root,
            file_name: "test.bin".to_string(),
            file_hash: [0u8; 32],
            total_size_bytes: 32 * n as u64,
            chunk_count: n,
            chunks,
        }
    }

    fn cbor(m: &DataManifest) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(m, &mut buf).unwrap();
        buf
    }

    fn root_hex(root: &[u8; 32]) -> String {
        root.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn validate_accepts_well_formed_manifest() {
        let manifest = well_formed_manifest(4, 0);
        let bytes = cbor(&manifest);
        let parsed = validate_manifest_push(&root_hex(&manifest.merkle_root), &bytes).unwrap();
        assert_eq!(parsed.merkle_root, manifest.merkle_root);
    }

    #[test]
    fn validate_rejects_root_mismatch() {
        let manifest = well_formed_manifest(4, 0);
        let bytes = cbor(&manifest);
        // Push the bytes under a DIFFERENT hex root.
        let mut wrong_root = manifest.merkle_root;
        wrong_root[0] ^= 0xFF;
        let err = validate_manifest_push(&root_hex(&wrong_root), &bytes).unwrap_err();
        assert!(err.contains("merkle_root mismatch"), "err = {err}");
    }

    #[test]
    fn validate_rejects_bad_hex_length() {
        let manifest = well_formed_manifest(1, 0);
        let bytes = cbor(&manifest);
        let err = validate_manifest_push("abcd", &bytes).unwrap_err();
        assert!(err.contains("invalid manifest root hex"), "err = {err}");
    }

    #[test]
    fn validate_rejects_garbage_cbor() {
        let root = [0u8; 32];
        let err = validate_manifest_push(&root_hex(&root), b"not cbor at all").unwrap_err();
        assert!(err.contains("deserialization failed"), "err = {err}");
    }

    /// chunk_count header doesn't match the chunks vector length.
    #[test]
    fn validate_rejects_chunk_count_mismatch() {
        let mut manifest = well_formed_manifest(4, 0);
        // Lie about the count.
        manifest.chunk_count = 5;
        let bytes = cbor(&manifest);
        let err = validate_manifest_push(&root_hex(&manifest.merkle_root), &bytes).unwrap_err();
        assert!(err.contains("chunk_count mismatch"), "err = {err}");
    }

    /// Chunk indices out of order — the merkle leaves are taken in
    /// vector order, so reordering would break root recomputation; we
    /// catch the misordering explicitly first for a clearer error.
    #[test]
    fn validate_rejects_misordered_indices() {
        let mut manifest = well_formed_manifest(4, 0);
        manifest.chunks.swap(1, 2);
        let bytes = cbor(&manifest);
        let err = validate_manifest_push(&root_hex(&manifest.merkle_root), &bytes).unwrap_err();
        assert!(err.contains("chunks out of order"), "err = {err}");
    }

    /// A chunk's `cid` doesn't match its declared `blake3_hash`. This
    /// matters because the receiver indexes `cid → root` and then
    /// uses that lookup as ACL truth — letting an attacker bind an
    /// arbitrary CID to a real root would let them gate any pull.
    #[test]
    fn validate_rejects_cid_blake3_mismatch() {
        let mut manifest = well_formed_manifest(2, 0);
        manifest.chunks[1].cid =
            "bafkr4ihaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let bytes = cbor(&manifest);
        let err = validate_manifest_push(&root_hex(&manifest.merkle_root), &bytes).unwrap_err();
        assert!(
            err.contains("cid does not match its blake3_hash"),
            "err = {err}"
        );
    }

    /// The most important test for this hardening pass: a manifest
    /// whose `merkle_root` field matches the CID, AND whose individual
    /// `chunk[i].cid ↔ blake3_hash` pairings are self-consistent, but
    /// whose chunk *contents* are forged — pointing at CIDs from a
    /// completely different file. Without merkle-root recomputation,
    /// such a manifest poisons the receiver's `cid → root` index.
    #[test]
    fn validate_rejects_forged_chunks_with_correct_root_field() {
        // Real manifest computed honestly.
        let real = well_formed_manifest(3, 0xAA);
        let real_root = real.merkle_root;

        // Forged manifest: declare the same root, but stuff in chunks
        // from a *different* file (different `seed` ⇒ different leaves
        // ⇒ different recomputed root). The blake3_hash ↔ cid pairings
        // are internally consistent (we built them honestly), so
        // checks 5 and 6 pass; only the recomputed-root check (7)
        // catches it.
        let other = well_formed_manifest(3, 0x55);
        assert_ne!(other.merkle_root, real_root, "fixture sanity: other != real");

        let forged = DataManifest {
            merkle_root: real_root,                 // claim to be `real`'s root
            file_name: real.file_name.clone(),
            file_hash: real.file_hash,
            total_size_bytes: other.total_size_bytes,
            chunk_count: other.chunk_count,
            chunks: other.chunks,                   // ← chunks from a DIFFERENT file
        };

        let bytes = cbor(&forged);
        let err = validate_manifest_push(&root_hex(&real_root), &bytes).unwrap_err();
        assert!(
            err.contains("merkle root recomputation failed"),
            "err = {err}"
        );
    }
}
