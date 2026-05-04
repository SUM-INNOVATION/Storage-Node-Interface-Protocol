//! Private file download (Phase 4b).
//!
//! Decrypts a Phase 4a Private file by:
//!
//!   1. Looking up the caller's `AccessEntryV2` on chain (with
//!      pagination if the access list spans multiple pages).
//!   2. Refusing if the entry is missing, has no `encrypted_key_bundle`,
//!      or has expired (`finalized_height > expires_at`).
//!   3. Unwrapping `K_file` from the bundle using the caller's X25519
//!      private key (derived from the Ed25519 seed via the same HKDF
//!      domain Phase 4a registers on chain).
//!   4. Fetching the encrypted manifest blob from the swarm and
//!      decrypting it with `K_file`.
//!   5. Defensively asserting the decrypted manifest's root matches the
//!      chain root and that its per-chunk ciphertext hashes Merkle-rebuild
//!      to that root.
//!   6. Fetching each ciphertext chunk (existing chunk-on-wire BLAKE3
//!      check already validates bytes-as-stored), decrypting with
//!      `decrypt_chunk`, and verifying `plaintext_blake3_hash`.
//!   7. Assembling the plaintext output and checking
//!      `manifest.file_hash`.
//!
//! Public V2 / V1 / not-found / RPC-unsupported files MUST never reach
//! this module — `run_download` in `main.rs` only routes here when
//! `storage_getFileInfoV2` returns a Private V2 row, otherwise it falls
//! through to the existing Public `DownloadOrchestrator` unchanged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{info, warn};
use zeroize::Zeroizing;

use sum_crypto::{
    decrypt_chunk, decrypt_manifest, unwrap_for_self,
    x25519_keypair_from_ed25519_seed, RECIPIENT_BUNDLE_SIZE,
};
use sum_net::{Keypair, PeerId, SumNet, SumNetEvent};
use sum_store::manifest::deserialize_manifest_cbor;
use sum_store::merkle::MerkleTree;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;
use sum_types::rpc_types::{AccessEntryV2, StorageFileInfoV2};
use sum_types::storage::{ChunkDescriptor, DataManifest};

use crate::peer_state::apply_peer_event;
use crate::rpc_client::L1RpcClient;

// ── Public types ─────────────────────────────────────────────────────────────

/// Errors surfaced by the Private download path. Each variant pins a
/// distinct failure mode the operator (or a test) can match against —
/// silently returning a generic `anyhow` would hide real diagnostic
/// signal (e.g. "bundle unwrap failed" vs "ciphertext tampered").
#[derive(Debug, Error)]
pub enum PrivateDownloadError {
    #[error(
        "requester {addr_b58} has no access entry on file {root}; \
         ask the file owner to AddAccess this address first"
    )]
    NoAccess { addr_b58: String, root: String },

    #[error(
        "--key-file is required for Private downloads (no X25519 secret \
         available to unwrap K_file)"
    )]
    NoKeyMaterial,

    #[error(
        "access entry exists but carries no encrypted_key_bundle — chain \
         rule should reject this at registration; refusing to proceed"
    )]
    NoBundle,

    #[error(
        "access expired: expires_at={expires_at} (finalized height {current} > expires_at)"
    )]
    AccessExpired { expires_at: u64, current: u64 },

    #[error(
        "encrypted_key_bundle wire shape invalid (expected {RECIPIENT_BUNDLE_SIZE}-byte hex \
         with optional 0x prefix): {reason}"
    )]
    BundleHex { reason: String },

    #[error(
        "failed to unwrap K_file from access bundle (wrong derived X25519 \
         key, or bundle tampered with): {0}"
    )]
    BundleUnwrap(#[source] sum_crypto::CryptoError),

    #[error("encrypted manifest fetch failed: {0}")]
    ManifestFetch(#[source] anyhow::Error),

    #[error(
        "manifest decryption failed (wrong K_file? tampered manifest?): {0}"
    )]
    ManifestDecrypt(#[source] sum_crypto::CryptoError),

    #[error("manifest CBOR parse failed: {0}")]
    ManifestParse(String),

    #[error("manifest's merkle_root {got} != chain merkle_root {expected}")]
    ManifestRootMismatch { got: String, expected: String },

    #[error("manifest's per-chunk ciphertext hashes do not Merkle-rebuild to the chain root")]
    ManifestMerkleMismatch,

    #[error(
        "Private chunk {idx} missing required plaintext_blake3_hash — \
         a Phase 4a Private file always carries Some(_); refusing"
    )]
    MissingPlaintextHash { idx: u32 },

    #[error("ciphertext fetch failed for chunk {idx}: {source}")]
    ChunkFetch { idx: u32, source: anyhow::Error },

    #[error("chunk {idx} decryption failed (wrong K_file? tampered ciphertext?): {source}")]
    ChunkDecrypt {
        idx: u32,
        #[source]
        source: sum_crypto::CryptoError,
    },

    #[error("chunk {idx} plaintext hash mismatch")]
    PlaintextHashMismatch { idx: u32 },

    #[error("whole-file plaintext hash mismatch (got {got}, expected {expected})")]
    FileHashMismatch { got: String, expected: String },

    #[error("output IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("RPC error: {0}")]
    Rpc(#[source] anyhow::Error),
}

// ── Pure helpers (testable without networking) ───────────────────────────────

/// Walk the access list (with pagination) until the entry whose
/// `address` matches `my_addr_b58` is found, or the chain reports the
/// list is exhausted. Returns the matching entry on success.
///
/// Implementation detail: chain returns a paginated slice via
/// `storage_getFileInfoV2(root, access_offset, access_limit)`. Each
/// page can contain up to `access_limit` entries (default 256). The
/// list is exhausted when a page returns fewer entries than the
/// requested limit. Uses an explicit `MAX_PAGES` safety cap so a
/// pathological chain response (e.g. perpetually returning a full
/// page) cannot turn this into an unbounded loop.
const ACCESS_PAGE_SIZE: u32 = 256;
const ACCESS_MAX_PAGES: u32 = 64; // 64 × 256 = 16,384 entries — well past any realistic file.

#[async_trait::async_trait]
pub trait AccessListSource: Send + Sync {
    async fn fetch_page(
        &self,
        root_hex: &str,
        offset: u32,
        limit: u32,
    ) -> Result<StorageFileInfoV2>;
}

#[async_trait::async_trait]
impl AccessListSource for L1RpcClient {
    async fn fetch_page(
        &self,
        root_hex: &str,
        offset: u32,
        limit: u32,
    ) -> Result<StorageFileInfoV2> {
        L1RpcClient::storage_get_file_info_v2(self, root_hex, Some(offset), Some(limit)).await
    }
}

/// Search every page of the access list for the caller's entry. The
/// FIRST page is provided up front (callers usually already have it
/// from the V2 dispatch probe in `run_download`); only paginate if
/// needed.
pub async fn find_my_access_entry<R: AccessListSource>(
    rpc: &R,
    root_hex: &str,
    my_addr_b58: &str,
    first_page: &StorageFileInfoV2,
) -> Result<AccessEntryV2, PrivateDownloadError> {
    if let Some(entry) = first_page
        .access_list
        .iter()
        .find(|e| e.address == my_addr_b58)
    {
        return Ok(entry.clone());
    }
    // First page didn't contain us. If the page was full, paginate.
    if first_page.access_list.len() < ACCESS_PAGE_SIZE as usize {
        return Err(PrivateDownloadError::NoAccess {
            addr_b58: my_addr_b58.to_string(),
            root: root_hex.to_string(),
        });
    }
    for page_idx in 1..ACCESS_MAX_PAGES {
        let offset = page_idx * ACCESS_PAGE_SIZE;
        let page = rpc
            .fetch_page(root_hex, offset, ACCESS_PAGE_SIZE)
            .await
            .map_err(PrivateDownloadError::Rpc)?;
        if let Some(entry) = page.access_list.iter().find(|e| e.address == my_addr_b58) {
            return Ok(entry.clone());
        }
        // A short page means the list is exhausted.
        if page.access_list.len() < ACCESS_PAGE_SIZE as usize {
            break;
        }
    }
    Err(PrivateDownloadError::NoAccess {
        addr_b58: my_addr_b58.to_string(),
        root: root_hex.to_string(),
    })
}

/// Strict-greater expiry rule (matches the chain's ACL semantics).
/// `entry.expires_at == None` means never-expires → always Ok.
pub fn check_access_expiry(
    entry: &AccessEntryV2,
    finalized_height: u64,
) -> Result<(), PrivateDownloadError> {
    match entry.expires_at {
        None => Ok(()),
        Some(expires_at) if finalized_height > expires_at => {
            Err(PrivateDownloadError::AccessExpired {
                expires_at,
                current: finalized_height,
            })
        }
        Some(_) => Ok(()),
    }
}

/// Parse the on-wire bundle hex into the fixed `[u8; 80]` shape.
/// Tolerates both `0x`-prefixed and bare hex (defensive — chain commits
/// only to the prefixed form, but accepting bare lets older RPC layers
/// keep working).
pub fn parse_bundle_hex(s: &str) -> Result<[u8; RECIPIENT_BUNDLE_SIZE], PrivateDownloadError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).map_err(|e| PrivateDownloadError::BundleHex {
        reason: format!("not valid hex: {e}"),
    })?;
    bytes.as_slice().try_into().map_err(|_| PrivateDownloadError::BundleHex {
        reason: format!("expected {RECIPIENT_BUNDLE_SIZE} bytes, got {}", bytes.len()),
    })
}

/// Decrypt the on-disk manifest blob and verify its root matches the
/// chain root and that its per-chunk hashes Merkle-rebuild correctly.
///
/// The Merkle rebuild is belt-and-braces: a peer that served the wrong
/// encrypted bytes would already trip the bundle-decryption tag check;
/// but if a future SNIP version ever weakens that guarantee (e.g. by
/// keying the AEAD differently), the on-disk content is still pinned to
/// the chain by the merkle root the chain commits to.
pub fn decrypt_and_verify_manifest(
    k_file: &Zeroizing<[u8; 32]>,
    encrypted_bytes: &[u8],
    chain_root: [u8; 32],
) -> Result<DataManifest, PrivateDownloadError> {
    let plaintext = decrypt_manifest(k_file, encrypted_bytes)
        .map_err(PrivateDownloadError::ManifestDecrypt)?;
    let manifest = deserialize_manifest_cbor(&plaintext)
        .map_err(|e| PrivateDownloadError::ManifestParse(e.to_string()))?;
    if manifest.merkle_root != chain_root {
        return Err(PrivateDownloadError::ManifestRootMismatch {
            got: hex::encode(manifest.merkle_root),
            expected: hex::encode(chain_root),
        });
    }
    let leaves: Vec<blake3::Hash> = manifest
        .chunks
        .iter()
        .map(|c| blake3::Hash::from(c.blake3_hash))
        .collect();
    let rebuilt = MerkleTree::build(&leaves);
    if rebuilt.root().as_bytes() != &chain_root {
        return Err(PrivateDownloadError::ManifestMerkleMismatch);
    }
    Ok(manifest)
}

/// Decrypt one ciphertext chunk and verify its plaintext hash. Phase
/// 4a always writes `Some(_)` for `plaintext_blake3_hash`; `None` here
/// means a malicious or future-format Private file → hard error.
pub fn decrypt_and_verify_chunk(
    k_file: &Zeroizing<[u8; 32]>,
    descriptor: &ChunkDescriptor,
    ciphertext: &[u8],
) -> Result<Vec<u8>, PrivateDownloadError> {
    let plaintext = decrypt_chunk(k_file, descriptor.chunk_index, ciphertext).map_err(|e| {
        PrivateDownloadError::ChunkDecrypt {
            idx: descriptor.chunk_index,
            source: e,
        }
    })?;
    let expected_pt_hash = descriptor
        .plaintext_blake3_hash
        .ok_or(PrivateDownloadError::MissingPlaintextHash {
            idx: descriptor.chunk_index,
        })?;
    let actual_pt_hash = *blake3::hash(&plaintext).as_bytes();
    if actual_pt_hash != expected_pt_hash {
        return Err(PrivateDownloadError::PlaintextHashMismatch {
            idx: descriptor.chunk_index,
        });
    }
    Ok(plaintext)
}

/// Verify the assembled output's whole-file plaintext hash matches
/// the manifest's `file_hash`.
pub fn check_file_hash(
    output_path: &Path,
    expected: [u8; 32],
) -> Result<(), PrivateDownloadError> {
    let mut hasher = blake3::Hasher::new();
    let mut f = std::fs::File::open(output_path)?;
    std::io::copy(&mut f, &mut hasher)?;
    let actual = *hasher.finalize().as_bytes();
    if actual != expected {
        return Err(PrivateDownloadError::FileHashMismatch {
            got: hex::encode(actual),
            expected: hex::encode(expected),
        });
    }
    Ok(())
}

// ── Production orchestrator ──────────────────────────────────────────────────

/// Run the Phase 4b Private download path. Caller (in `main.rs`) is
/// responsible for ensuring `info` is a Private V2 row.
#[allow(clippy::too_many_arguments)]
pub async fn run_download_private(
    keypair: Keypair,
    seed: [u8; 32],
    rpc: Arc<L1RpcClient>,
    net: Arc<SumNet>,
    info: StorageFileInfoV2,
    chain_root: [u8; 32],
    output: PathBuf,
    max_concurrent: usize,
    timeout: Duration,
) -> Result<()> {
    let root_hex = format!("0x{}", hex::encode(chain_root));
    let my_addr = sum_net::identity::l1_address_from_keypair(&keypair);
    let my_addr_b58 = sum_net::identity::l1_address_base58(&my_addr);

    info!(
        root = %root_hex,
        addr = %my_addr_b58,
        output = %output.display(),
        "starting Private download"
    );

    // ── Step 2: find my access entry ────────────────────────────────
    let entry = find_my_access_entry(rpc.as_ref(), &root_hex, &my_addr_b58, &info).await?;

    // ── Step 3: require bundle ──────────────────────────────────────
    let bundle_hex = entry
        .encrypted_key_bundle
        .as_ref()
        .ok_or(PrivateDownloadError::NoBundle)?;
    let bundle = parse_bundle_hex(bundle_hex)?;

    // ── Step 4: expiry (finalized) ──────────────────────────────────
    if entry.expires_at.is_some() {
        let head = rpc
            .chain_get_block_height()
            .await
            .map_err(PrivateDownloadError::Rpc)?;
        check_access_expiry(&entry, head.height)?;
    }

    // ── Step 5: derive X25519 secret ────────────────────────────────
    let (x25519_secret_bytes, _x25519_pubkey) = x25519_keypair_from_ed25519_seed(&seed);
    let x25519_secret: Zeroizing<[u8; 32]> = Zeroizing::new(x25519_secret_bytes);
    // Original (un-zeroized) `x25519_secret_bytes` is on the stack; we
    // can't intercept its drop here, but the `keypair_from_seed`
    // computation produces it once and we move it into the zeroizing
    // wrapper immediately, so its plaintext lifetime is one expression.

    // ── Step 6: unwrap K_file ───────────────────────────────────────
    let k_file_bytes = unwrap_for_self(&bundle, &x25519_secret, &my_addr)
        .map_err(PrivateDownloadError::BundleUnwrap)?;
    let k_file: Zeroizing<[u8; 32]> = Zeroizing::new(k_file_bytes);

    // ── Step 7: fetch encrypted manifest ────────────────────────────
    let peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let encrypted_manifest_bytes =
        fetch_encrypted_manifest(net.as_ref(), &peer_addresses, &chain_root, timeout)
            .await
            .map_err(PrivateDownloadError::ManifestFetch)?;

    // ── Step 8–11: decrypt manifest, verify root + Merkle ──────────
    let manifest = decrypt_and_verify_manifest(&k_file, &encrypted_manifest_bytes, chain_root)?;
    info!(
        chunks = manifest.chunks.len(),
        plaintext_size = manifest.total_size_bytes,
        "Private manifest decrypted and verified"
    );

    // ── Step 12: fetch ciphertext chunks (V2-assignment-aware) ──────
    let _ = max_concurrent; // sequential per chunk for now; per-chunk
                            // parallelism is left to Phase 4d polish.
    let ciphertext_chunks = fetch_all_ciphertext_chunks_v2(
        net.as_ref(),
        rpc.as_ref(),
        &peer_addresses,
        &info,
        &manifest,
        timeout,
    )
    .await
    .map_err(|(idx, source)| PrivateDownloadError::ChunkFetch { idx, source })?;

    // ── Step 13–15: decrypt chunks + plaintext hash + assemble ──────
    let mut out = std::fs::File::create(&output).map_err(PrivateDownloadError::Io)?;
    use std::io::Write;
    for cd in manifest.chunks.iter() {
        let ct = ciphertext_chunks
            .get(&cd.chunk_index)
            .ok_or_else(|| PrivateDownloadError::ChunkFetch {
                idx: cd.chunk_index,
                source: anyhow::anyhow!("chunk fetch returned no bytes for index"),
            })?;
        let plaintext = decrypt_and_verify_chunk(&k_file, cd, ct)?;
        out.write_all(&plaintext).map_err(PrivateDownloadError::Io)?;
    }
    out.flush().map_err(PrivateDownloadError::Io)?;
    drop(out);

    // ── Step 16: whole-file hash ────────────────────────────────────
    check_file_hash(&output, manifest.file_hash)?;

    info!(
        chunks = manifest.chunks.len(),
        output = %output.display(),
        "Private download complete (file_hash verified)"
    );
    net.shutdown().await.ok();
    Ok(())
}

// ── Private network helpers ──────────────────────────────────────────────────
//
// These mostly mirror the equivalent loops in `download.rs`. We're
// duplicating rather than refactoring because the user explicitly asked
// not to regress V1/Public downloads — touching the existing
// `DownloadOrchestrator` to share peer-discovery / manifest-fetch code
// is more invasive than the win is worth for a single new caller.

async fn fetch_encrypted_manifest(
    net: &SumNet,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    chain_root: &[u8; 32],
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let root_hex = hex::encode(chain_root);
    let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{root_hex}");
    let mut tried: HashSet<PeerId> = HashSet::new();
    let mut discovered_peers: Vec<PeerId> = Vec::new();

    loop {
        tokio::select! {
            event = net.next_event() => {
                match event {
                    Some(SumNetEvent::PeerDiscovered { peer_id, .. }) => {
                        if !discovered_peers.contains(&peer_id) {
                            discovered_peers.push(peer_id);
                            try_request_manifest(net, peer_id, &root_hex, &mut tried).await;
                        }
                    }
                    Some(ref e @ SumNetEvent::PeerIdentified { .. })
                    | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                        apply_peer_event(&mut *peer_addresses.write().await, e);
                    }
                    Some(SumNetEvent::ShardReceived { peer_id, response }) if response.cid == manifest_cid => {
                        if let Some(err) = response.error.as_deref() {
                            warn!(%peer_id, %err, "Private manifest fetch: peer rejected — waiting for others");
                            continue;
                        }
                        return Ok(response.data);
                    }
                    None => anyhow::bail!("network shut down before Private manifest received"),
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                anyhow::bail!("Private manifest fetch timed out after {timeout:?}");
            }
        }
    }
}

async fn try_request_manifest(
    net: &SumNet,
    peer_id: PeerId,
    root_hex: &str,
    tried: &mut HashSet<PeerId>,
) {
    if tried.insert(peer_id) {
        if let Err(e) = net
            .request_manifest(peer_id, root_hex.to_string())
            .await
        {
            warn!(%peer_id, %e, "Private manifest request enqueue failed");
        }
    }
}

/// V2-assignment-aware multi-peer ciphertext chunk fetch.
///
/// Private files are distributed across the V2 deterministic
/// assignment: each chunk lives on a specific subset of `R` archives
/// (per `chain_getChainParams.assignment_replication_factor`). The
/// downloader MUST request each chunk from one of *its* assigned
/// archives — picking an arbitrary peer would fail for any
/// non-trivial multi-archive file because the peer simply wouldn't
/// hold the chunks not assigned to it.
///
/// This helper:
///   1. Reads the active-archive snapshot at `info.assignment_height`
///      and the chain's replication factor.
///   2. Computes the per-chunk assigned-archive list deterministically
///      via `sum_store::assignment_v2::assigned_archives`.
///   3. Drives a single event loop that:
///      - tracks per-chunk attempts across its assigned archives;
///      - sends a request to the first resolvable assigned archive
///        for each pending chunk;
///      - on `PeerIdentified`, refreshes the L1-addr→PeerId map and
///        retries any chunks whose assigned archives just became
///        resolvable;
///      - on a valid `ShardReceived` (ciphertext hash matches the
///        manifest descriptor) records the chunk;
///      - on a wrong-bytes or error response, marks that archive as
///        Failed for that chunk and moves to the next assigned
///        archive (or fails the chunk if all are exhausted).
///
/// Single-peer sequential fan-out within a chunk's assigned set is
/// fine for first-cut Phase 4b; per-chunk concurrency is left to
/// Phase 4d polish.
async fn fetch_all_ciphertext_chunks_v2(
    net: &SumNet,
    rpc: &L1RpcClient,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    info: &StorageFileInfoV2,
    manifest: &DataManifest,
    timeout: Duration,
) -> std::result::Result<HashMap<u32, Vec<u8>>, (u32, anyhow::Error)> {
    let deadline = tokio::time::Instant::now() + timeout;

    // ── Step 12a: build the V2 assignment view ─────────────────────
    let snapshot_records = rpc
        .storage_get_active_nodes_at_height(info.assignment_height)
        .await
        .map_err(|e| (0u32, anyhow::anyhow!("storage_getActiveNodesAtHeight: {e}")))?;
    let mut snapshot: Vec<[u8; 20]> = Vec::with_capacity(snapshot_records.len());
    for n in &snapshot_records {
        let addr = sum_net::identity::l1_address_from_base58(&n.address)
            .map_err(|e| (0u32, anyhow::anyhow!("snapshot l1 address parse: {e}")))?;
        snapshot.push(addr);
    }
    snapshot.sort();
    if snapshot.is_empty() {
        return Err((
            0,
            anyhow::anyhow!(
                "snapshot at assignment_height={} has no active archives — cannot route chunk requests",
                info.assignment_height
            ),
        ));
    }
    let chain_params = rpc
        .chain_get_chain_params()
        .await
        .map_err(|e| (0u32, anyhow::anyhow!("chain_getChainParams: {e}")))?;
    let r = chain_params.assignment_replication_factor;

    // Per-chunk assigned archives (chain-deterministic, byte-for-byte
    // identical to chain validation).
    let merkle_root = manifest.merkle_root;
    let mut state: HashMap<u32, ChunkFetchState> = HashMap::with_capacity(manifest.chunks.len());
    for cd in &manifest.chunks {
        let assigned = sum_store::assignment_v2::assigned_archives(
            &merkle_root,
            &snapshot,
            cd.chunk_index,
            r,
        );
        if assigned.is_empty() {
            return Err((
                cd.chunk_index,
                anyhow::anyhow!(
                    "chunk {} has no V2-assigned archives (snapshot empty?)",
                    cd.chunk_index
                ),
            ));
        }
        state.insert(
            cd.chunk_index,
            ChunkFetchState {
                assigned,
                next_attempt_idx: 0,
                in_flight_to: None,
                received: None,
            },
        );
    }

    // CID lookup for response routing.
    let cid_to_idx: HashMap<String, u32> = manifest
        .chunks
        .iter()
        .map(|c| (c.cid.clone(), c.chunk_index))
        .collect();

    // Helper: try to dispatch as many pending chunks as possible. A
    // chunk is dispatchable when it is not yet `received`, has no
    // outstanding `in_flight_to`, has at least one untried assigned
    // archive, AND the next-untried archive is currently resolvable
    // to a known PeerId.
    async fn try_dispatch_pending(
        net: &SumNet,
        peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
        manifest: &DataManifest,
        state: &mut HashMap<u32, ChunkFetchState>,
    ) -> Result<(), (u32, anyhow::Error)> {
        let map = peer_addresses.read().await.clone();
        for cd in &manifest.chunks {
            let s = state.get_mut(&cd.chunk_index).expect("seeded above");
            if s.received.is_some() || s.in_flight_to.is_some() {
                continue;
            }
            // Walk untried archives until we hit one we can resolve.
            while s.next_attempt_idx < s.assigned.len() {
                let target_addr = s.assigned[s.next_attempt_idx];
                if let Some(peer_id) = map
                    .iter()
                    .find_map(|(p, a)| if a == &target_addr { Some(*p) } else { None })
                {
                    if let Err(e) = net
                        .request_shard_chunk(peer_id, cd.cid.clone(), None, None)
                        .await
                    {
                        // Send-side failure: don't burn an archive
                        // attempt — surface the error.
                        return Err((
                            cd.chunk_index,
                            anyhow::anyhow!("request_shard_chunk: {e}"),
                        ));
                    }
                    s.in_flight_to = Some((peer_id, target_addr));
                    s.next_attempt_idx += 1;
                    break;
                } else {
                    // Unresolvable right now — leave alone; a future
                    // PeerIdentified event will let us pick it up.
                    break;
                }
            }
        }
        Ok(())
    }

    // Initial dispatch attempt (peer_addresses may already have
    // entries from prior phases of `run_download_private`; usually
    // not, since the Private path hasn't done peer discovery yet).
    try_dispatch_pending(net, peer_addresses, manifest, &mut state).await?;

    // Main event loop.
    while state.values().any(|s| s.received.is_none()) {
        let event = tokio::select! {
            ev = net.next_event() => ev,
            _ = tokio::time::sleep_until(deadline) => {
                let next_missing = manifest
                    .chunks
                    .iter()
                    .map(|c| c.chunk_index)
                    .find(|i| state.get(i).is_some_and(|s| s.received.is_none()))
                    .unwrap_or(0);
                let s = state.get(&next_missing);
                let detail = s
                    .map(|s| format!(
                        "assigned={} archives, tried={}, in_flight={:?}",
                        s.assigned.len(),
                        s.next_attempt_idx,
                        s.in_flight_to.map(|(_, a)| hex::encode(a)),
                    ))
                    .unwrap_or_default();
                return Err((
                    next_missing,
                    anyhow::anyhow!(
                        "Private chunk fetch timed out after {timeout:?}; chunk {next_missing} pending ({detail})"
                    ),
                ));
            }
        };
        match event {
            Some(SumNetEvent::PeerDiscovered { .. }) => {
                // Peer discovery on its own doesn't unlock the
                // L1-addr→PeerId map — we wait for PeerIdentified for
                // that. No-op here.
            }
            Some(ref e @ SumNetEvent::PeerIdentified { .. })
            | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                apply_peer_event(&mut *peer_addresses.write().await, e);
                // A new PeerIdentified may have unlocked archives we
                // couldn't resolve before. Dispatch any newly-eligible
                // chunks.
                try_dispatch_pending(net, peer_addresses, manifest, &mut state).await?;
            }
            Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                let Some(&idx) = cid_to_idx.get(&response.cid) else {
                    // Not a chunk we're waiting on (or already fulfilled).
                    continue;
                };
                let s = state.get_mut(&idx).expect("idx came from manifest");
                if s.received.is_some() {
                    // Already done — ignore stragglers.
                    continue;
                }
                // Only count responses from the peer we asked. Stale
                // / unsolicited responses don't burn an archive attempt.
                let Some((expected_peer, attempted_addr)) = s.in_flight_to else {
                    continue;
                };
                if peer_id != expected_peer {
                    continue;
                }
                s.in_flight_to = None;
                if let Some(err) = response.error.as_deref() {
                    warn!(
                        chunk_index = idx,
                        peer = %peer_id,
                        archive = %hex::encode(attempted_addr),
                        %err,
                        "Private chunk fetch: peer error, trying next assigned archive"
                    );
                    // Fall through to dispatch (advances next_attempt_idx, picks next archive).
                } else {
                    // Defensive ciphertext-hash check (chain commits to
                    // these bytes via the merkle root).
                    let cd = manifest
                        .chunks
                        .iter()
                        .find(|c| c.chunk_index == idx)
                        .expect("idx came from manifest");
                    let actual_hash = *blake3::hash(&response.data).as_bytes();
                    if actual_hash != cd.blake3_hash {
                        warn!(
                            chunk_index = idx,
                            peer = %peer_id,
                            archive = %hex::encode(attempted_addr),
                            got = %hex::encode(actual_hash),
                            expected = %hex::encode(cd.blake3_hash),
                            "Private chunk fetch: peer served wrong bytes, trying next assigned archive"
                        );
                        // Fall through to dispatch.
                    } else {
                        s.received = Some(response.data);
                        continue;
                    }
                }
                // Failure path: re-dispatch this chunk to its next assigned archive.
                if s.next_attempt_idx >= s.assigned.len() {
                    return Err((
                        idx,
                        anyhow::anyhow!(
                            "all {} V2-assigned archives for chunk {idx} failed",
                            s.assigned.len()
                        ),
                    ));
                }
                try_dispatch_pending(net, peer_addresses, manifest, &mut state).await?;
            }
            None => {
                let next_missing = manifest
                    .chunks
                    .iter()
                    .map(|c| c.chunk_index)
                    .find(|i| state.get(i).is_some_and(|s| s.received.is_none()))
                    .unwrap_or(0);
                return Err((
                    next_missing,
                    anyhow::anyhow!("network shut down mid-fetch"),
                ));
            }
            _ => {}
        }
    }

    let mut out = HashMap::with_capacity(manifest.chunks.len());
    for (idx, s) in state {
        out.insert(idx, s.received.expect("loop exits when all received"));
    }
    Ok(out)
}

/// Per-chunk fetch bookkeeping for `fetch_all_ciphertext_chunks_v2`.
struct ChunkFetchState {
    /// V2-deterministic list of archives that hold this chunk, in
    /// (score asc, address asc) order.
    assigned: Vec<[u8; 20]>,
    /// Index of the next archive in `assigned` we'll try.
    next_attempt_idx: usize,
    /// `(peer_id, archive_l1_addr)` of the in-flight request, if any.
    in_flight_to: Option<(PeerId, [u8; 20])>,
    /// Validated ciphertext bytes once received.
    received: Option<Vec<u8>>,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_crypto::{
        encrypt_chunk, encrypt_manifest, wrap_for_recipient,
        x25519_keypair_from_ed25519_seed as derive_x25519,
    };
    use sum_types::storage::ChunkDescriptor;

    fn fake_kfile() -> Zeroizing<[u8; 32]> {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = ((i as u8) ^ 0x5A).wrapping_mul(7);
        }
        Zeroizing::new(k)
    }

    fn fake_addr(b: u8) -> [u8; 20] {
        [b; 20]
    }

    // Build a one-chunk Private manifest + matching ciphertext for a
    // 100-byte plaintext. Returns (manifest, encrypted_manifest_bytes,
    // ciphertexts indexed by chunk_index, plaintext bytes).
    fn build_one_chunk_fixture(
        k_file: &Zeroizing<[u8; 32]>,
    ) -> (DataManifest, Vec<u8>, HashMap<u32, Vec<u8>>, Vec<u8>) {
        let plaintext: Vec<u8> = (0..100u8).collect();
        let pt_hash = *blake3::hash(&plaintext).as_bytes();
        let ct = encrypt_chunk(k_file, 0, &plaintext);
        let ct_hash = *blake3::hash(&ct).as_bytes();
        let cid = sum_store::cid_from_data(&ct);

        let chunk = ChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: ct.len() as u64,
            blake3_hash: ct_hash,
            cid,
            plaintext_blake3_hash: Some(pt_hash),
        };
        let leaves = vec![blake3::Hash::from(ct_hash)];
        let merkle_root = *MerkleTree::build(&leaves).root().as_bytes();
        let file_hash = *blake3::hash(&plaintext).as_bytes();
        let manifest = DataManifest {
            file_name: "fixture.bin".into(),
            file_hash,
            total_size_bytes: plaintext.len() as u64,
            chunk_count: 1,
            merkle_root,
            chunks: vec![chunk],
        };

        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&manifest, &mut cbor).unwrap();
        let encrypted_manifest = encrypt_manifest(k_file, &cbor);

        let mut chunks_map = HashMap::new();
        chunks_map.insert(0u32, ct);
        (manifest, encrypted_manifest, chunks_map, plaintext)
    }

    // ── parse_bundle_hex ────────────────────────────────────────────

    #[test]
    fn parse_bundle_hex_accepts_prefixed_and_bare() {
        let prefixed = format!("0x{}", "11".repeat(80));
        let bare = "11".repeat(80);
        assert!(parse_bundle_hex(&prefixed).is_ok());
        assert!(parse_bundle_hex(&bare).is_ok());
    }

    #[test]
    fn parse_bundle_hex_rejects_wrong_length() {
        assert!(matches!(
            parse_bundle_hex(&"11".repeat(40)),
            Err(PrivateDownloadError::BundleHex { .. })
        ));
    }

    #[test]
    fn parse_bundle_hex_rejects_non_hex() {
        assert!(matches!(
            parse_bundle_hex("0xZZZZ"),
            Err(PrivateDownloadError::BundleHex { .. })
        ));
    }

    // ── check_access_expiry ─────────────────────────────────────────

    #[test]
    fn check_access_expiry_none_means_never() {
        let entry = AccessEntryV2 {
            address: "abc".into(),
            encrypted_key_bundle: None,
            expires_at: None,
        };
        assert!(check_access_expiry(&entry, u64::MAX).is_ok());
    }

    #[test]
    fn check_access_expiry_strict_greater_rule() {
        let entry = AccessEntryV2 {
            address: "abc".into(),
            encrypted_key_bundle: None,
            expires_at: Some(100),
        };
        // current == expires_at → still valid (strict >).
        assert!(check_access_expiry(&entry, 100).is_ok());
        // current > expires_at → expired.
        match check_access_expiry(&entry, 101) {
            Err(PrivateDownloadError::AccessExpired {
                expires_at: 100,
                current: 101,
            }) => (),
            other => panic!("expected AccessExpired, got {other:?}"),
        }
    }

    // ── decrypt_and_verify_manifest ─────────────────────────────────

    #[test]
    fn decrypt_and_verify_manifest_happy_path() {
        let k = fake_kfile();
        let (manifest, encrypted, _, _) = build_one_chunk_fixture(&k);
        let recovered =
            decrypt_and_verify_manifest(&k, &encrypted, manifest.merkle_root).expect("decrypt");
        assert_eq!(recovered.merkle_root, manifest.merkle_root);
        assert_eq!(recovered.chunks.len(), 1);
    }

    #[test]
    fn decrypt_and_verify_manifest_wrong_kfile_rejects() {
        let k = fake_kfile();
        let (manifest, encrypted, _, _) = build_one_chunk_fixture(&k);
        let mut wrong = [0u8; 32];
        wrong[0] = 1;
        let wrong_k = Zeroizing::new(wrong);
        assert!(matches!(
            decrypt_and_verify_manifest(&wrong_k, &encrypted, manifest.merkle_root),
            Err(PrivateDownloadError::ManifestDecrypt(_))
        ));
    }

    #[test]
    fn decrypt_and_verify_manifest_root_mismatch_rejects() {
        let k = fake_kfile();
        let (_, encrypted, _, _) = build_one_chunk_fixture(&k);
        let bogus_root = [0xFFu8; 32];
        assert!(matches!(
            decrypt_and_verify_manifest(&k, &encrypted, bogus_root),
            Err(PrivateDownloadError::ManifestRootMismatch { .. })
        ));
    }

    // ── decrypt_and_verify_chunk ────────────────────────────────────

    #[test]
    fn decrypt_and_verify_chunk_happy_path() {
        let k = fake_kfile();
        let (manifest, _, ciphertexts, plaintext) = build_one_chunk_fixture(&k);
        let cd = &manifest.chunks[0];
        let ct = &ciphertexts[&0u32];
        let recovered = decrypt_and_verify_chunk(&k, cd, ct).expect("decrypt");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn decrypt_and_verify_chunk_tampered_ciphertext_rejects() {
        let k = fake_kfile();
        let (manifest, _, ciphertexts, _) = build_one_chunk_fixture(&k);
        let cd = &manifest.chunks[0];
        let mut ct = ciphertexts[&0u32].clone();
        ct[0] ^= 0x80; // bit-flip plaintext byte 0
        assert!(matches!(
            decrypt_and_verify_chunk(&k, cd, &ct),
            Err(PrivateDownloadError::ChunkDecrypt { idx: 0, .. })
        ));
    }

    #[test]
    fn decrypt_and_verify_chunk_missing_plaintext_hash_rejects() {
        let k = fake_kfile();
        let (manifest, _, ciphertexts, _) = build_one_chunk_fixture(&k);
        let mut cd = manifest.chunks[0].clone();
        cd.plaintext_blake3_hash = None; // simulate malicious / future-format manifest
        let ct = &ciphertexts[&0u32];
        assert!(matches!(
            decrypt_and_verify_chunk(&k, &cd, ct),
            Err(PrivateDownloadError::MissingPlaintextHash { idx: 0 })
        ));
    }

    #[test]
    fn decrypt_and_verify_chunk_wrong_plaintext_hash_rejects() {
        let k = fake_kfile();
        let (manifest, _, ciphertexts, _) = build_one_chunk_fixture(&k);
        let mut cd = manifest.chunks[0].clone();
        cd.plaintext_blake3_hash = Some([0u8; 32]); // wrong hash
        let ct = &ciphertexts[&0u32];
        assert!(matches!(
            decrypt_and_verify_chunk(&k, &cd, ct),
            Err(PrivateDownloadError::PlaintextHashMismatch { idx: 0 })
        ));
    }

    // ── find_my_access_entry (with mock RPC) ────────────────────────

    struct MockAccessRpc {
        pages: Vec<Vec<AccessEntryV2>>,
        calls: std::sync::Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl AccessListSource for MockAccessRpc {
        async fn fetch_page(
            &self,
            _root_hex: &str,
            offset: u32,
            _limit: u32,
        ) -> Result<StorageFileInfoV2> {
            *self.calls.lock().unwrap() += 1;
            let page_idx = (offset / ACCESS_PAGE_SIZE) as usize;
            let page = self.pages.get(page_idx).cloned().unwrap_or_default();
            Ok(StorageFileInfoV2 {
                merkle_root: "0x00".into(),
                owner: "owner".into(),
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 0,
                fee_pool: 0,
                created_at: 0,
                activated_at_height: None,
                abandoned_at_height: None,
                assignment_height: 0,
                visibility: sum_types::rpc_types::VisibilityV2::PRIVATE,
                lifecycle: sum_types::rpc_types::LifecycleV2::ACTIVE,
                access_list: page,
            })
        }
    }

    fn entry(addr: &str, expires_at: Option<u64>) -> AccessEntryV2 {
        AccessEntryV2 {
            address: addr.into(),
            encrypted_key_bundle: Some(format!("0x{}", "AB".repeat(80))),
            expires_at,
        }
    }

    #[tokio::test]
    async fn find_my_access_entry_first_page_hit() {
        let rpc = MockAccessRpc {
            pages: vec![vec![entry("alice", None), entry("bob", None)]],
            calls: std::sync::Mutex::new(0),
        };
        let first_page = rpc.fetch_page("root", 0, ACCESS_PAGE_SIZE).await.unwrap();
        let got = find_my_access_entry(&rpc, "root", "bob", &first_page)
            .await
            .unwrap();
        assert_eq!(got.address, "bob");
        // No additional pagination calls beyond the seed page (which we
        // fetched manually here).
        assert_eq!(*rpc.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn find_my_access_entry_paginates_when_full_first_page() {
        // 256 entries on page 0 (full), target on page 1.
        let mut page0: Vec<AccessEntryV2> = (0..256)
            .map(|i| entry(&format!("u{i:03}"), None))
            .collect();
        // Make sure target isn't on page 0.
        for e in &mut page0 {
            assert_ne!(e.address, "target");
        }
        let page1 = vec![entry("target", None)];
        let rpc = MockAccessRpc {
            pages: vec![page0, page1],
            calls: std::sync::Mutex::new(0),
        };
        let first_page = rpc.fetch_page("root", 0, ACCESS_PAGE_SIZE).await.unwrap();
        let got = find_my_access_entry(&rpc, "root", "target", &first_page)
            .await
            .unwrap();
        assert_eq!(got.address, "target");
        // 1 seed + at least 1 paginated call.
        assert!(*rpc.calls.lock().unwrap() >= 2);
    }

    #[tokio::test]
    async fn find_my_access_entry_no_access_short_first_page() {
        // First page is short (< ACCESS_PAGE_SIZE) and doesn't contain
        // us → list is exhausted, return NoAccess immediately without
        // paginating.
        let rpc = MockAccessRpc {
            pages: vec![vec![entry("alice", None), entry("bob", None)]],
            calls: std::sync::Mutex::new(0),
        };
        let first_page = rpc.fetch_page("root", 0, ACCESS_PAGE_SIZE).await.unwrap();
        let calls_before = *rpc.calls.lock().unwrap();
        let err = find_my_access_entry(&rpc, "root", "missing", &first_page)
            .await
            .unwrap_err();
        assert!(matches!(err, PrivateDownloadError::NoAccess { .. }));
        // No additional fetch_page calls beyond the seed.
        assert_eq!(*rpc.calls.lock().unwrap(), calls_before);
    }

    // ── End-to-end round trip with synthetic ciphertext ─────────────

    #[test]
    fn round_trip_owner_can_recover_via_helpers() {
        // Owner derives X25519 from a fixed Ed25519 seed; wraps K_file
        // for themselves; download path unwraps and decrypts.
        let owner_seed = [0xAA; 32];
        let (owner_sk, owner_pk) = derive_x25519(&owner_seed);
        let owner_addr = fake_addr(0xCD);

        // K_file lives in zeroizing memory throughout.
        let k_file = fake_kfile();

        // Bundle for owner.
        let bundle = wrap_for_recipient(&k_file, &owner_addr, &owner_pk).unwrap();

        // Recover K_file via download path.
        let owner_sk_z: Zeroizing<[u8; 32]> = Zeroizing::new(owner_sk);
        let recovered = unwrap_for_self(&bundle, &owner_sk_z, &owner_addr).unwrap();
        let recovered_z: Zeroizing<[u8; 32]> = Zeroizing::new(recovered);
        assert_eq!(*recovered_z, *k_file);

        // Decrypt + verify a synthetic manifest + chunk under that key.
        let (manifest, encrypted, ciphertexts, plaintext) = build_one_chunk_fixture(&k_file);
        let m = decrypt_and_verify_manifest(&recovered_z, &encrypted, manifest.merkle_root)
            .expect("manifest decrypts under recovered K_file");
        let pt = decrypt_and_verify_chunk(&recovered_z, &m.chunks[0], &ciphertexts[&0u32])
            .expect("chunk decrypts");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn shared_recipient_can_unwrap_and_decrypt() {
        // Two recipients (owner + R). Verify R's key works.
        let r_seed = [0xBB; 32];
        let (r_sk, r_pk) = derive_x25519(&r_seed);
        let r_addr = fake_addr(0xEF);
        let k_file = fake_kfile();

        let bundle_for_r = wrap_for_recipient(&k_file, &r_addr, &r_pk).unwrap();
        let r_sk_z: Zeroizing<[u8; 32]> = Zeroizing::new(r_sk);
        let recovered = unwrap_for_self(&bundle_for_r, &r_sk_z, &r_addr).unwrap();
        assert_eq!(recovered, *k_file);
    }

    #[test]
    fn unauthorized_peer_cannot_unwrap_owners_bundle() {
        // Bundle is wrapped for owner; attacker has a different keypair
        // and attempts to unwrap with their own X25519 secret. Must fail.
        let owner_seed = [0xAA; 32];
        let (_, owner_pk) = derive_x25519(&owner_seed);
        let owner_addr = fake_addr(0xCD);
        let attacker_seed = [0x99; 32];
        let (attacker_sk, _) = derive_x25519(&attacker_seed);
        let k_file = fake_kfile();

        let bundle_for_owner = wrap_for_recipient(&k_file, &owner_addr, &owner_pk).unwrap();
        let attacker_sk_z: Zeroizing<[u8; 32]> = Zeroizing::new(attacker_sk);
        // Attempt to unwrap with attacker's secret — even if they
        // somehow got the bundle bytes, the AEAD tag fails.
        assert!(matches!(
            unwrap_for_self(&bundle_for_owner, &attacker_sk_z, &owner_addr),
            Err(sum_crypto::CryptoError::DecryptionFailed)
        ));
    }

    /// V2 multi-peer fetch routing: per chunk, the assigned-archive
    /// list comes from `sum_store::assignment_v2::assigned_archives`
    /// against the file's `assignment_height` snapshot — chain
    /// deterministic, byte-identical to chain validation. This test
    /// pins the routing-correctness invariant the reviewer asked for:
    /// for a typical multi-archive Private file, each chunk's
    /// assigned set is non-empty, equals R (clamped to snapshot
    /// size), and chunks at distinct indices land on different
    /// archive subsets (i.e. no single peer holds every chunk —
    /// which was the exact bug the prior single-peer fetch had).
    #[test]
    fn v2_per_chunk_routing_distributes_chunks_across_archives() {
        // 5-archive snapshot, R=3, 8 chunks.
        let snapshot: Vec<[u8; 20]> = (1u8..=5).map(|i| [i; 20]).collect();
        let merkle_root = [0xAB; 32];
        let r = 3u32;
        let chunk_indices: Vec<u32> = (0..8).collect();

        let mut all_assigned: Vec<Vec<[u8; 20]>> = Vec::new();
        let mut union: std::collections::HashSet<[u8; 20]> = std::collections::HashSet::new();
        for &idx in &chunk_indices {
            let assigned = sum_store::assignment_v2::assigned_archives(
                &merkle_root,
                &snapshot,
                idx,
                r,
            );
            assert_eq!(
                assigned.len(),
                r as usize,
                "chunk {idx}: assigned set size must equal R"
            );
            for a in &assigned {
                union.insert(*a);
            }
            all_assigned.push(assigned);
        }

        // Sanity: with 5 archives and R=3 across 8 chunks, the union
        // of all assigned sets MUST cover every archive at least
        // once. If a single peer happened to be assigned to every
        // chunk, that'd be a 1-element union and we'd be back to the
        // original single-peer bug.
        assert_eq!(
            union.len(),
            snapshot.len(),
            "union of assigned archives across all chunks must cover the full snapshot, \
             otherwise the V2 routing fix is moot — single-peer fetch would still suffice"
        );

        // Determinism: re-running with the same inputs yields the
        // same per-chunk lists.
        for (i, expected) in all_assigned.iter().enumerate() {
            let again = sum_store::assignment_v2::assigned_archives(
                &merkle_root,
                &snapshot,
                i as u32,
                r,
            );
            assert_eq!(&again, expected, "chunk {i} routing must be deterministic");
        }
    }

    /// `R` is clamped to the snapshot size: `assigned_archives` with
    /// R > |snapshot| returns the full snapshot, not a panic. The V2
    /// fetch helper relies on this so a mis-tuned chain `R` doesn't
    /// trip an out-of-bounds.
    #[test]
    fn v2_routing_clamps_replication_factor_to_snapshot_size() {
        let snapshot: Vec<[u8; 20]> = (1u8..=3).map(|i| [i; 20]).collect();
        let merkle_root = [0xAB; 32];
        // R = 7 against 3-archive snapshot → clamped to 3.
        let assigned =
            sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, 0, 7);
        assert_eq!(assigned.len(), 3);
    }
}

