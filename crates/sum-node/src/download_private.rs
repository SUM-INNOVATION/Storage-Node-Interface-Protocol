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

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{info, warn};
use zeroize::Zeroizing;

use sum_crypto::{
    RECIPIENT_BUNDLE_SIZE, decrypt_chunk, decrypt_manifest, unwrap_for_self,
    x25519_keypair_from_ed25519_seed,
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

    #[error("access expired: expires_at={expires_at} (finalized height {current} > expires_at)")]
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
        "encrypted manifest fetch exhausted all V2-assigned archives: tried \
         {tried} of {assigned_total} ({resolvable} resolvable, {unresolvable} \
         unresolvable); last error: {last_reason}"
    )]
    ManifestFetchAllArchivesFailed {
        /// Total V2-assigned archives for this file — the union of
        /// `assigned_archives(merkle_root, snapshot, chunk_index, R)` across
        /// every chunk index. Mirrors the upload-side `distinct_assigned`
        /// set so the caller is asking the same archives that hold the file.
        assigned_total: usize,
        /// Archives we issued a `request_manifest` to before giving up.
        /// Equal to the number of distinct archives whose PeerId we resolved
        /// (each resolvable archive is dispatched at most once).
        tried: usize,
        /// Archives whose PeerId resolved at any point before failure.
        resolvable: usize,
        /// Archives whose PeerId never resolved before the deadline (peer
        /// not yet identified, possibly offline).
        unresolvable: usize,
        /// Last per-response failure surfaced (validation rejection,
        /// peer-side error string, or "deadline exceeded"). Diagnostic
        /// hint only — not a structured cause.
        last_reason: String,
    },

    #[error("manifest decryption failed (wrong K_file? tampered manifest?): {0}")]
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
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| PrivateDownloadError::BundleHex {
            reason: format!(
                "expected {RECIPIENT_BUNDLE_SIZE} bytes, got {}",
                bytes.len()
            ),
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
    let plaintext =
        decrypt_manifest(k_file, encrypted_bytes).map_err(PrivateDownloadError::ManifestDecrypt)?;
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
    let expected_pt_hash =
        descriptor
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
pub fn check_file_hash(output_path: &Path, expected: [u8; 32]) -> Result<(), PrivateDownloadError> {
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

    // ── Step 7–11: fetch + verify manifest (V2 fan-out) ────────────
    //
    // Phase 4d: bounded fan-out across the V2 distinct_assigned set.
    // Inline validation (decrypt + chain-root + Merkle) means this
    // returns a `DataManifest` directly — Step 8–11 of the original
    // Phase 4b plan are now folded in. Caller MUST NOT re-validate.
    let peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let manifest = fetch_manifest_v2(
        net.as_ref(),
        rpc.as_ref(),
        &peer_addresses,
        &info,
        chain_root,
        &k_file,
        timeout,
        max_concurrent,
    )
    .await?;
    info!(
        chunks = manifest.chunks.len(),
        plaintext_size = manifest.total_size_bytes,
        "Private manifest fetched and verified"
    );

    // ── Step 12: fetch ciphertext chunks (V2-assignment-aware) ──────
    //
    // `max_concurrent` is the hard cap on simultaneous in-flight
    // chunk requests. Phase 4d wires it through; Phase 4b accepted
    // it but ignored it (sequential, one chunk at a time per peer).
    // Clamp to >= 1 so a misconfigured `--max-concurrent 0` doesn't
    // hang forever waiting for slot 0 to free up.
    let max_in_flight = max_concurrent.max(1);
    let ciphertext_chunks = fetch_all_ciphertext_chunks_v2(
        net.as_ref(),
        rpc.as_ref(),
        &peer_addresses,
        &info,
        &manifest,
        timeout,
        max_in_flight,
    )
    .await
    .map_err(|(idx, source)| PrivateDownloadError::ChunkFetch { idx, source })?;

    // ── Step 13–15: decrypt chunks + plaintext hash + assemble ──────
    let mut out = std::fs::File::create(&output).map_err(PrivateDownloadError::Io)?;
    use std::io::Write;
    for cd in manifest.chunks.iter() {
        let ct = ciphertext_chunks.get(&cd.chunk_index).ok_or_else(|| {
            PrivateDownloadError::ChunkFetch {
                idx: cd.chunk_index,
                source: anyhow::anyhow!("chunk fetch returned no bytes for index"),
            }
        })?;
        let plaintext = decrypt_and_verify_chunk(&k_file, cd, ct)?;
        out.write_all(&plaintext)
            .map_err(PrivateDownloadError::Io)?;
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

/// V2-assignment-aware manifest fetch with bounded fan-out and inline
/// validation (Phase 4d).
///
/// Replaces the Phase 4b single-peer manifest fetch. The Phase 4b
/// version accepted the FIRST manifest response from any connected
/// peer (peer-discovered, not assignment-aware) and validated it
/// AFTER returning. Two consequences:
///
///   * A slow / malicious peer in the assigned set could stall fetch
///     even when other archives could serve immediately.
///   * Wrong-root or undecryptable responses surfaced as a hard error
///     instead of a per-peer rejection — one bad responder poisoned
///     the whole download.
///
/// Phase 4d behavior:
///
///   1. Compute the V2 distinct_assigned set: the union of
///      `assigned_archives(merkle_root, snapshot, chunk_index, R)` for
///      every chunk index in `[0, info.chunk_count)`. This mirrors
///      the upload-side push set in `s2_push_chunks` byte-for-byte —
///      the same archives that received the manifest at upload are
///      asked for it at download.
///   2. Maintain a `ManifestArchiveStatus` per archive (Untried /
///      Dispatched / Failed). Dispatch up to `fanout =
///      compute_manifest_fanout(max_concurrent, |distinct_assigned|)`
///      requests in flight at a time, picking sorted-by-address.
///   3. On `ShardReceived` for the manifest CID from a peer we
///      dispatched to: validate inline (decrypt with K_file, parse,
///      check chain root, rebuild Merkle root). First archive whose
///      response validates wins.
///   4. On rejection (peer-side error string, decrypt fail, parse
///      fail, root mismatch, Merkle mismatch): mark archive `Failed`,
///      log a per-archive warn, dispatch a replacement so the
///      in-flight cap stays full.
///   5. Drop unsolicited responses (CID mismatch, sender not a peer
///      we asked, archive already transitioned to Failed) silently —
///      same hygiene as chunk fetch.
///   6. On all archives `Failed` (no `Untried` and no `Dispatched`
///      left) OR on deadline: return
///      `PrivateDownloadError::ManifestFetchAllArchivesFailed` with
///      structured tried / resolvable / unresolvable counts and the
///      last rejection reason.
///
/// Returns the already-decrypted, root-and-Merkle-verified
/// `DataManifest`. Caller MUST NOT re-validate.
async fn fetch_manifest_v2(
    net: &SumNet,
    rpc: &L1RpcClient,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    info: &StorageFileInfoV2,
    chain_root: [u8; 32],
    k_file: &Zeroizing<[u8; 32]>,
    timeout: Duration,
    max_concurrent: usize,
) -> std::result::Result<DataManifest, PrivateDownloadError> {
    // ── 1. Build V2 distinct_assigned ───────────────────────────────
    //
    // Mirrors `fetch_all_ciphertext_chunks_v2` Step 12a — duplicated
    // inline rather than extracted to keep the manifest-fan-out diff
    // local to this function (chunk fetch is intentionally untouched
    // by this slice). A small follow-up could lift snapshot+R into a
    // shared helper if the duplication starts hurting.
    let snapshot_records = rpc
        .storage_get_active_nodes_at_height(info.assignment_height)
        .await
        .map_err(|e| {
            PrivateDownloadError::ManifestFetch(anyhow::anyhow!(
                "storage_getActiveNodesAtHeight: {e}"
            ))
        })?;
    let mut snapshot: Vec<[u8; 20]> = Vec::with_capacity(snapshot_records.len());
    for n in &snapshot_records {
        let addr = sum_net::identity::l1_address_from_base58(&n.address).map_err(|e| {
            PrivateDownloadError::ManifestFetch(anyhow::anyhow!("snapshot l1 address parse: {e}"))
        })?;
        snapshot.push(addr);
    }
    snapshot.sort();
    if snapshot.is_empty() {
        return Err(PrivateDownloadError::ManifestFetch(anyhow::anyhow!(
            "snapshot at assignment_height={} has no active archives — \
             cannot route manifest request",
            info.assignment_height
        )));
    }
    let chain_params = rpc.chain_get_chain_params().await.map_err(|e| {
        PrivateDownloadError::ManifestFetch(anyhow::anyhow!("chain_getChainParams: {e}"))
    })?;
    let r = chain_params.assignment_replication_factor;

    let mut distinct_assigned: BTreeSet<[u8; 20]> = BTreeSet::new();
    for chunk_index in 0..info.chunk_count {
        let assigned =
            sum_store::assignment_v2::assigned_archives(&chain_root, &snapshot, chunk_index, r);
        for addr in assigned {
            distinct_assigned.insert(addr);
        }
    }
    if distinct_assigned.is_empty() {
        return Err(PrivateDownloadError::ManifestFetch(anyhow::anyhow!(
            "file has 0 V2-assigned archives (chunk_count={}); cannot fetch manifest",
            info.chunk_count
        )));
    }
    let assigned_total = distinct_assigned.len();
    let fanout = compute_manifest_fanout(max_concurrent, assigned_total);

    // ── 2. Per-archive state + bookkeeping ──────────────────────────
    let mut archive_status: HashMap<[u8; 20], ManifestArchiveStatus> = distinct_assigned
        .iter()
        .map(|a| (*a, ManifestArchiveStatus::Untried))
        .collect();
    // Reverse map peer_id → archive_addr for the (small) set of peers
    // we've dispatched to. Lets us reject `ShardReceived` from peers
    // we never asked without scanning archive_status.
    let mut dispatched_peers: HashMap<PeerId, [u8; 20]> = HashMap::new();
    let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", hex::encode(chain_root));
    let root_hex = hex::encode(chain_root);
    // Last per-response failure surfaced; embedded in the structured
    // error if every archive is exhausted.
    let mut last_reason: String = "no responses received".to_string();

    // Helper: snapshot peer_addresses, run the pure selector, fire
    // `request_manifest`, and transition archive_status. Mirrors the
    // chunk-concurrency `try_dispatch_pending` style.
    async fn dispatch_wave(
        net: &SumNet,
        peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
        archive_status: &mut HashMap<[u8; 20], ManifestArchiveStatus>,
        dispatched_peers: &mut HashMap<PeerId, [u8; 20]>,
        root_hex: &str,
        fanout: usize,
    ) {
        let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
            .read()
            .await
            .iter()
            .map(|(p, a)| (*a, *p))
            .collect();
        let dispatches = select_manifest_dispatch(archive_status, &addr_to_peer, fanout);
        for d in dispatches {
            match net.request_manifest(d.peer_id, root_hex.to_string()).await {
                Ok(()) => {
                    archive_status.insert(d.archive_addr, ManifestArchiveStatus::Dispatched);
                    dispatched_peers.insert(d.peer_id, d.archive_addr);
                }
                Err(e) => {
                    // Send-side enqueue failure: mark this archive
                    // failed (it never reached the wire). Prefer
                    // marking-Failed over leaving Untried so we don't
                    // try the same dispatch repeatedly on every
                    // dispatch wave.
                    warn!(
                        peer = %d.peer_id,
                        archive = %hex::encode(d.archive_addr),
                        %e,
                        "Private manifest fan-out: enqueue failed; marking archive failed"
                    );
                    archive_status.insert(d.archive_addr, ManifestArchiveStatus::Failed);
                }
            }
        }
    }

    // Helper: build the structured all-failed error. Used by every
    // non-success exit path (deadline, network shutdown, archives
    // exhausted) so the diagnostic shape is identical.
    let build_all_failed = |archive_status: &HashMap<[u8; 20], ManifestArchiveStatus>,
                            addr_to_peer: &HashMap<[u8; 20], PeerId>,
                            last_reason: String|
     -> PrivateDownloadError {
        let resolvable = distinct_assigned
            .iter()
            .filter(|a| addr_to_peer.contains_key(*a))
            .count();
        let unresolvable = assigned_total - resolvable;
        let tried = archive_status
            .values()
            .filter(|s| !matches!(s, ManifestArchiveStatus::Untried))
            .count();
        PrivateDownloadError::ManifestFetchAllArchivesFailed {
            assigned_total,
            tried,
            resolvable,
            unresolvable,
            last_reason,
        }
    };

    // ── 3. Initial dispatch ────────────────────────────────────────
    dispatch_wave(
        net,
        peer_addresses,
        &mut archive_status,
        &mut dispatched_peers,
        &root_hex,
        fanout,
    )
    .await;

    // ── 4. Event loop ──────────────────────────────────────────────
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Termination: every archive has been Failed (no Untried
        // waiting on PeerIdentified, no Dispatched in-flight). We
        // intentionally keep looping while ANY archive is Untried —
        // it may yet become resolvable via PeerIdentified before the
        // deadline.
        let any_alive = archive_status
            .values()
            .any(|s| !matches!(s, ManifestArchiveStatus::Failed));
        if !any_alive {
            let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                .read()
                .await
                .iter()
                .map(|(p, a)| (*a, *p))
                .collect();
            return Err(build_all_failed(
                &archive_status,
                &addr_to_peer,
                last_reason,
            ));
        }

        let event = tokio::select! {
            ev = net.next_event() => ev,
            _ = tokio::time::sleep_until(deadline) => {
                let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                    .read()
                    .await
                    .iter()
                    .map(|(p, a)| (*a, *p))
                    .collect();
                return Err(build_all_failed(
                    &archive_status,
                    &addr_to_peer,
                    format!("manifest fetch deadline exceeded after {timeout:?}; previous: {last_reason}"),
                ));
            }
        };

        match event {
            Some(SumNetEvent::PeerDiscovered { .. }) => {
                // PeerDiscovered alone doesn't unlock the
                // L1-addr→PeerId map; PeerIdentified does.
            }
            Some(ref e @ SumNetEvent::PeerIdentified { .. })
            | Some(ref e @ SumNetEvent::PeerDisconnected { .. }) => {
                apply_peer_event(&mut *peer_addresses.write().await, e);
                // A new PeerIdentified may have unlocked an Untried
                // archive — try to fill the in-flight quota.
                dispatch_wave(
                    net,
                    peer_addresses,
                    &mut archive_status,
                    &mut dispatched_peers,
                    &root_hex,
                    fanout,
                )
                .await;
            }
            Some(SumNetEvent::ShardReceived { peer_id, response }) => {
                if response.cid != manifest_cid {
                    // Not the manifest CID — likely a chunk response
                    // we don't care about right now (Step 12 handles
                    // chunks). Drop silently.
                    continue;
                }
                let Some(&archive_addr) = dispatched_peers.get(&peer_id) else {
                    // Sender wasn't on our dispatch list — could be
                    // stale or unsolicited. Drop.
                    continue;
                };
                if !matches!(
                    archive_status.get(&archive_addr),
                    Some(ManifestArchiveStatus::Dispatched)
                ) {
                    // Already transitioned (duplicate response, or a
                    // race we don't expect). Drop.
                    continue;
                }

                if let Some(err) = response.error.as_deref() {
                    warn!(
                        peer = %peer_id,
                        archive = %hex::encode(archive_addr),
                        %err,
                        "Private manifest fan-out: peer-side error; trying others"
                    );
                    last_reason =
                        format!("archive {} peer error: {}", hex::encode(archive_addr), err);
                    archive_status.insert(archive_addr, ManifestArchiveStatus::Failed);
                } else {
                    match decrypt_and_verify_manifest(k_file, &response.data, chain_root) {
                        Ok(manifest) => {
                            // First valid response wins. Subsequent
                            // ShardReceived events for the manifest CID
                            // (from still-in-flight peers) drain into
                            // the next caller's `next_event()` and are
                            // dropped there as unrecognized CIDs.
                            return Ok(manifest);
                        }
                        Err(e) => {
                            warn!(
                                peer = %peer_id,
                                archive = %hex::encode(archive_addr),
                                err = %e,
                                "Private manifest fan-out: validation rejected response; trying others"
                            );
                            last_reason =
                                format!("archive {} validation: {}", hex::encode(archive_addr), e);
                            archive_status.insert(archive_addr, ManifestArchiveStatus::Failed);
                        }
                    }
                }

                // Replace the failed in-flight slot so the next-fastest
                // archive isn't blocked on the slow one.
                dispatch_wave(
                    net,
                    peer_addresses,
                    &mut archive_status,
                    &mut dispatched_peers,
                    &root_hex,
                    fanout,
                )
                .await;
            }
            None => {
                let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
                    .read()
                    .await
                    .iter()
                    .map(|(p, a)| (*a, *p))
                    .collect();
                return Err(build_all_failed(
                    &archive_status,
                    &addr_to_peer,
                    format!("network shut down mid-fetch; previous: {last_reason}"),
                ));
            }
            _ => {}
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
/// Phase 4d enforces a per-fetch in-flight cap (`max_concurrent`):
/// no more than that many chunks have outstanding requests at any
/// moment. Within a chunk's assigned-archive set the fan-out is
/// still sequential (try archive 0 → fail → try archive 1 → …);
/// concurrency happens *across* chunks, not within a single chunk.
/// `max_concurrent` is the cap on chunks-in-flight, not peers-in-use.
async fn fetch_all_ciphertext_chunks_v2(
    net: &SumNet,
    rpc: &L1RpcClient,
    peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
    info: &StorageFileInfoV2,
    manifest: &DataManifest,
    timeout: Duration,
    max_concurrent: usize,
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
        let assigned =
            sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, cd.chunk_index, r);
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

    // Helper: pick chunks to dispatch (subject to `max_concurrent`),
    // call `request_shard_chunk` for each, and mark them in-flight.
    // The pure selection logic lives in `select_chunks_to_dispatch`
    // below so concurrency invariants can be tested without standing
    // up a real `SumNet`.
    async fn try_dispatch_pending(
        net: &SumNet,
        peer_addresses: &RwLock<HashMap<PeerId, [u8; 20]>>,
        manifest: &DataManifest,
        state: &mut HashMap<u32, ChunkFetchState>,
        max_concurrent: usize,
    ) -> Result<(), (u32, anyhow::Error)> {
        // Snapshot the L1-addr → PeerId map ONCE per dispatch wave so
        // selection is deterministic for this wave.
        let addr_to_peer: HashMap<[u8; 20], PeerId> = peer_addresses
            .read()
            .await
            .iter()
            .map(|(p, a)| (*a, *p))
            .collect();

        let dispatches =
            select_chunks_to_dispatch(state, &manifest.chunks, &addr_to_peer, max_concurrent);

        for d in dispatches {
            // Find the chunk's CID — we already validated `idx` came
            // from the manifest in `select_chunks_to_dispatch`.
            let cid = manifest
                .chunks
                .iter()
                .find(|c| c.chunk_index == d.chunk_index)
                .expect("idx came from manifest")
                .cid
                .clone();
            if let Err(e) = net.request_shard_chunk(d.peer_id, cid, None, None).await {
                // Send-side failure: surface the error. State is
                // unchanged (we haven't marked in_flight_to yet), so
                // a future dispatch attempt can retry the same
                // (chunk, archive) pair without burning the archive.
                return Err((d.chunk_index, anyhow::anyhow!("request_shard_chunk: {e}")));
            }
            // Mark in-flight ONLY after the wire send succeeded so a
            // failed `request_shard_chunk` doesn't burn an archive
            // attempt. Burning the archive would force the chunk to
            // skip a perfectly-good peer just because the local send
            // queue was momentarily full.
            let s = state
                .get_mut(&d.chunk_index)
                .expect("idx came from manifest");
            s.in_flight_to = Some((d.peer_id, d.archive_addr));
            s.next_attempt_idx += 1;
        }
        Ok(())
    }

    // Initial dispatch attempt (peer_addresses may already have
    // entries from prior phases of `run_download_private`; usually
    // not, since the Private path hasn't done peer discovery yet).
    try_dispatch_pending(net, peer_addresses, manifest, &mut state, max_concurrent).await?;

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
                try_dispatch_pending(net, peer_addresses, manifest, &mut state, max_concurrent)
                    .await?;
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
                try_dispatch_pending(net, peer_addresses, manifest, &mut state, max_concurrent)
                    .await?;
            }
            None => {
                let next_missing = manifest
                    .chunks
                    .iter()
                    .map(|c| c.chunk_index)
                    .find(|i| state.get(i).is_some_and(|s| s.received.is_none()))
                    .unwrap_or(0);
                return Err((next_missing, anyhow::anyhow!("network shut down mid-fetch")));
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

/// One scheduled dispatch produced by `select_chunks_to_dispatch`.
/// The caller (production: `try_dispatch_pending` calling `SumNet`;
/// tests: a recorder) is responsible for performing the actual
/// network send and marking the corresponding `ChunkFetchState` as
/// in-flight afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DispatchTarget {
    chunk_index: u32,
    peer_id: PeerId,
    archive_addr: [u8; 20],
}

/// **Pure** concurrency selector for the per-chunk fetch loop. Given
/// the current `state` map, the manifest's chunk descriptors (used
/// only to drive deterministic iteration order over `chunk_index`),
/// the L1-addr → PeerId resolution map, and a `max_concurrent` cap,
/// returns the list of chunks that should be dispatched right now.
///
/// Selection rules:
///   * Iterates chunks in `manifest.chunks` declaration order
///     (chunk_index 0 first), so under low concurrency the lowest
///     chunk indices fill in first — predictable for operators.
///   * Skips chunks that already have a value in `received` or an
///     outstanding `in_flight_to`. (Slow / stuck chunks therefore
///     do NOT block other pending chunks from progressing — they
///     simply hold one slot.)
///   * For each pending chunk, walks `assigned[next_attempt_idx..]`
///     and dispatches against the first archive resolvable to a
///     known PeerId. Failed-archive retries are folded in here:
///     the failure path elsewhere leaves `in_flight_to = None` and
///     `next_attempt_idx` advanced past the failed archive, so this
///     selector picks the next archive on the same chunk
///     transparently while other chunks continue.
///   * Stops as soon as the live in-flight count (existing in-flight
///     plus the dispatches selected this wave) reaches
///     `max_concurrent`.
///
/// Determinism is the load-bearing testability property: given the
/// same inputs this returns the exact same dispatch list, so the
/// concurrency invariants tested in
/// `select_chunks_to_dispatch_*` reflect production behavior 1:1.
fn select_chunks_to_dispatch(
    state: &HashMap<u32, ChunkFetchState>,
    manifest_chunks: &[ChunkDescriptor],
    addr_to_peer: &HashMap<[u8; 20], PeerId>,
    max_concurrent: usize,
) -> Vec<DispatchTarget> {
    let cap = max_concurrent.max(1);
    let mut in_flight = state.values().filter(|s| s.in_flight_to.is_some()).count();
    let mut out: Vec<DispatchTarget> = Vec::new();
    for cd in manifest_chunks {
        if in_flight >= cap {
            break;
        }
        let Some(s) = state.get(&cd.chunk_index) else {
            continue;
        };
        if s.received.is_some() || s.in_flight_to.is_some() {
            continue;
        }
        // Try only the next-untried archive. If its PeerId is not
        // resolvable yet, leave the chunk alone for this dispatch
        // wave — a future PeerIdentified event will let us pick it
        // up. We deliberately don't skip ahead to a later archive:
        // dispatching out of order would let a later archive serve
        // before an earlier one is given a chance.
        let probe_idx = s.next_attempt_idx;
        if probe_idx < s.assigned.len() {
            let target_addr = s.assigned[probe_idx];
            if let Some(&peer_id) = addr_to_peer.get(&target_addr) {
                out.push(DispatchTarget {
                    chunk_index: cd.chunk_index,
                    peer_id,
                    archive_addr: target_addr,
                });
                in_flight += 1;
            }
        }
    }
    out
}

// ── Phase 4d manifest fan-out: pure helpers ──────────────────────────────────

/// Per-archive manifest-fetch state, keyed by L1 address. Mirrors the
/// per-chunk `ChunkFetchState` but flatter: a single archive is in
/// exactly one of three states for the whole fetch (a manifest is
/// served-or-not; there's no archive-list-walking like there is per
/// chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestArchiveStatus {
    /// Eligible to be dispatched (PeerId may or may not yet be resolvable).
    Untried,
    /// `request_manifest` was issued; awaiting response or deadline.
    Dispatched,
    /// Response was received and rejected (validation failed, peer-side
    /// error, or wire shape malformed). Will not be retried — the V2
    /// assignment is deterministic, the SAME archive returning a SECOND
    /// response would be no more trustworthy than the first.
    Failed,
}

/// One scheduled manifest dispatch produced by `select_manifest_dispatch`.
/// The async wrapper performs the wire send and transitions the archive
/// from `Untried` to `Dispatched`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManifestDispatchTarget {
    peer_id: PeerId,
    archive_addr: [u8; 20],
}

/// Compute the manifest fan-out cap.
///
/// Per the operator-tunable constraint: `fanout = max_concurrent
/// .clamp(1, 3) .min(assigned_size)`. The clamp upper bound (3) reflects
/// that manifests are tiny and small-fan-out is enough to mask one slow
/// peer; raising it past 3 amplifies inbound bandwidth on archives
/// without buying meaningful resilience. The clamp lower bound (1) means
/// a misconfigured `--max-concurrent 0` still attempts at least one
/// archive instead of deadlocking. The `.min(assigned_size)` prevents
/// dispatching more requests than there are candidates.
///
/// `assigned_size == 0` returns `0` and is the caller's signal to bail
/// out before entering the fetch loop — we'd never make progress.
fn compute_manifest_fanout(max_concurrent: usize, assigned_size: usize) -> usize {
    if assigned_size == 0 {
        return 0;
    }
    max_concurrent.clamp(1, 3).min(assigned_size)
}

/// **Pure** dispatch selector for the manifest fan-out loop. Returns
/// the list of (peer, archive) pairs to dispatch right now, subject to
/// the live in-flight cap of `fanout`.
///
/// Selection rules:
///   * Iterates `Untried` archives in sorted-by-address order so
///     selection is deterministic across runs (operators see the same
///     archive picked first given the same inputs).
///   * Skips archives whose PeerId is not yet in `addr_to_peer`. Unlike
///     `select_chunks_to_dispatch`, here we DO skip-and-continue:
///     manifest archives are unordered (any can serve), so passing
///     over an unresolvable archive to dispatch a resolvable one
///     doesn't violate any priority invariant.
///   * Stops as soon as `(existing dispatched) + (selected this wave)
///     == fanout`.
fn select_manifest_dispatch(
    archive_status: &HashMap<[u8; 20], ManifestArchiveStatus>,
    addr_to_peer: &HashMap<[u8; 20], PeerId>,
    fanout: usize,
) -> Vec<ManifestDispatchTarget> {
    let in_flight = archive_status
        .values()
        .filter(|s| matches!(s, ManifestArchiveStatus::Dispatched))
        .count();
    if in_flight >= fanout {
        return Vec::new();
    }
    let mut remaining = fanout - in_flight;

    let mut untried: Vec<[u8; 20]> = archive_status
        .iter()
        .filter(|(_, s)| matches!(s, ManifestArchiveStatus::Untried))
        .map(|(a, _)| *a)
        .collect();
    untried.sort();

    let mut out: Vec<ManifestDispatchTarget> = Vec::new();
    for addr in untried {
        if remaining == 0 {
            break;
        }
        if let Some(&peer_id) = addr_to_peer.get(&addr) {
            out.push(ManifestDispatchTarget {
                peer_id,
                archive_addr: addr,
            });
            remaining -= 1;
        }
        // Unresolvable: skip — keep status as Untried so a future
        // PeerIdentified lets us pick it up next wave.
    }
    out
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
        let mut page0: Vec<AccessEntryV2> =
            (0..256).map(|i| entry(&format!("u{i:03}"), None)).collect();
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

    /// **Privacy-audit row #8 + #15 fail-closed pin.** When the
    /// access-list pagination RPC fails (network error, malformed
    /// response, JSON-RPC error), `find_my_access_entry` MUST surface
    /// the failure as a typed RPC error — NEVER as `NoAccess`. A
    /// silent degrade would conflate "not on the list" with "couldn't
    /// determine," masking real auth state both ways: a transient
    /// blip would deny a legitimate recipient, and an attacker
    /// spoofing chain errors could pretend to be authoritative about
    /// absence. Production profile must fail closed AND distinguish
    /// the two failure modes.
    #[tokio::test]
    async fn find_my_access_entry_fails_closed_on_rpc_error() {
        // First page returns a FULL access list (256 entries) without
        // the target, so the function paginates. The second page call
        // fails with a simulated RPC error — that's the branch under
        // test.
        struct FailingAccessRpc;

        #[async_trait::async_trait]
        impl AccessListSource for FailingAccessRpc {
            async fn fetch_page(
                &self,
                _root_hex: &str,
                offset: u32,
                _limit: u32,
            ) -> Result<StorageFileInfoV2> {
                if offset == 0 {
                    let full_page: Vec<AccessEntryV2> = (0..ACCESS_PAGE_SIZE)
                        .map(|i| entry(&format!("u{i:03}"), None))
                        .collect();
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
                        access_list: full_page,
                    })
                } else {
                    anyhow::bail!("simulated chain RPC failure on page offset={offset}")
                }
            }
        }

        let rpc = FailingAccessRpc;
        let first_page = rpc.fetch_page("root", 0, ACCESS_PAGE_SIZE).await.unwrap();
        let err = find_my_access_entry(&rpc, "root", "missing", &first_page)
            .await
            .expect_err("RPC failure during pagination must surface as Err");

        // Load-bearing assertion: typed Rpc, NOT NoAccess.
        match err {
            PrivateDownloadError::Rpc(_) => {}
            PrivateDownloadError::NoAccess { .. } => panic!(
                "regression: RPC failure was reported as NoAccess — \
                 fail-closed contract requires distinguishing 'not on list' \
                 from 'could not determine'"
            ),
            other => panic!(
                "expected PrivateDownloadError::Rpc, got: {other:?} \
                 (any non-Rpc variant must be deliberate; update this \
                 test if the contract changes)"
            ),
        }
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
            let assigned =
                sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, idx, r);
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
            let again =
                sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, i as u32, r);
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
        let assigned = sum_store::assignment_v2::assigned_archives(&merkle_root, &snapshot, 0, 7);
        assert_eq!(assigned.len(), 3);
    }

    // ── Phase 4d per-chunk concurrency: select_chunks_to_dispatch ──

    /// Build a synthetic manifest with `n` chunks. Chunk indices are
    /// 0..n; chunk hashes / cids are placeholder bytes — the
    /// concurrency selector only consults `chunk_index` for ordering,
    /// so the synthetic shape is enough.
    fn synth_manifest(n: u32) -> DataManifest {
        let chunks: Vec<ChunkDescriptor> = (0..n)
            .map(|i| ChunkDescriptor {
                chunk_index: i,
                offset: 0,
                size: 0,
                blake3_hash: [(i & 0xff) as u8; 32],
                cid: format!("bafk_concurrency_{i}"),
                plaintext_blake3_hash: Some([0xAA; 32]),
            })
            .collect();
        DataManifest {
            file_name: "concurrency-fixture.bin".into(),
            file_hash: [0; 32],
            total_size_bytes: 0,
            chunk_count: n,
            merkle_root: [0; 32],
            chunks,
        }
    }

    /// Build a `ChunkFetchState` map where every chunk has the same
    /// assigned archive set, all archives are resolvable to fake
    /// peers, and no chunk is yet in-flight or received.
    fn synth_state(
        n: u32,
        archives_per_chunk: &[[u8; 20]],
    ) -> (HashMap<u32, ChunkFetchState>, HashMap<[u8; 20], PeerId>) {
        let state: HashMap<u32, ChunkFetchState> = (0..n)
            .map(|i| {
                (
                    i,
                    ChunkFetchState {
                        assigned: archives_per_chunk.to_vec(),
                        next_attempt_idx: 0,
                        in_flight_to: None,
                        received: None,
                    },
                )
            })
            .collect();
        let addr_to_peer: HashMap<[u8; 20], PeerId> = archives_per_chunk
            .iter()
            .map(|addr| {
                (
                    *addr,
                    sum_net::Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect();
        (state, addr_to_peer)
    }

    /// `max_concurrent = 1` collapses the selector to one dispatch
    /// per wave — the existing pre-Phase-4d behavior. Pinning this
    /// guards against accidental concurrency leaks under operator
    /// configurations that explicitly want sequential.
    #[test]
    fn select_chunks_to_dispatch_max_concurrent_one_is_sequential() {
        let manifest = synth_manifest(5);
        let archives = vec![[0xA1u8; 20]];
        let (state, addr_to_peer) = synth_state(5, &archives);

        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 1);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].chunk_index, 0);
    }

    /// `max_concurrent = N` produces exactly N dispatches when N
    /// pending chunks are eligible. Iteration order is by chunk
    /// index ascending — operator-predictable.
    #[test]
    fn select_chunks_to_dispatch_max_concurrent_n_caps_in_flight() {
        let manifest = synth_manifest(10);
        let archives = vec![[0xA1u8; 20]];
        let (state, addr_to_peer) = synth_state(10, &archives);

        for n in [1usize, 2, 3, 5, 10] {
            let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, n);
            assert_eq!(dispatches.len(), n, "max_concurrent={n}");
            // Ascending chunk-index order.
            for (k, d) in dispatches.iter().enumerate() {
                assert_eq!(d.chunk_index as usize, k);
            }
        }
    }

    /// Existing in-flight dispatches count toward `max_concurrent`,
    /// AND a slow / stuck chunk does NOT block other chunks from
    /// proceeding — the selector simply skips it and dispatches the
    /// remaining slots. This is the load-bearing concurrency
    /// invariant the user's spec asked for.
    #[test]
    fn select_chunks_to_dispatch_slow_chunk_does_not_block_others() {
        let manifest = synth_manifest(5);
        let archives = vec![[0xA1u8; 20]];
        let (mut state, addr_to_peer) = synth_state(5, &archives);

        // Chunk 0 is in-flight ("slow"). Don't mark received.
        let stuck_peer = addr_to_peer[&[0xA1u8; 20]];
        state.get_mut(&0).unwrap().in_flight_to = Some((stuck_peer, [0xA1u8; 20]));
        state.get_mut(&0).unwrap().next_attempt_idx = 1;

        // With max_concurrent=3, we already have 1 in-flight (chunk 0)
        // → selector should add 2 more (chunks 1 and 2).
        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 3);
        assert_eq!(dispatches.len(), 2);
        let indices: Vec<u32> = dispatches.iter().map(|d| d.chunk_index).collect();
        assert_eq!(indices, vec![1, 2]);
    }

    /// After a chunk's first archive fails (event loop sets
    /// `in_flight_to = None` while leaving `next_attempt_idx`
    /// advanced past the failed archive), the next selector wave
    /// dispatches that chunk against archive 1 — without disturbing
    /// other chunks already in flight.
    #[test]
    fn select_chunks_to_dispatch_failed_archive_retries_next_assigned() {
        let manifest = synth_manifest(3);
        let archives = vec![[0xA1u8; 20], [0xA2u8; 20], [0xA3u8; 20]];
        let (mut state, addr_to_peer) = synth_state(3, &archives);

        // Chunk 0: archive[0] tried and failed (in_flight_to=None,
        // next_attempt_idx=1). Chunk 1: in-flight on archive[0].
        state.get_mut(&0).unwrap().next_attempt_idx = 1;
        let busy_peer = addr_to_peer[&[0xA1u8; 20]];
        state.get_mut(&1).unwrap().in_flight_to = Some((busy_peer, [0xA1u8; 20]));
        state.get_mut(&1).unwrap().next_attempt_idx = 1;

        // max_concurrent=3: 1 already in-flight → selector adds up
        // to 2 more. Chunk 0 should retry on archive[1]; chunk 2 on
        // archive[0].
        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 3);
        assert_eq!(dispatches.len(), 2);

        let chunk0 = dispatches
            .iter()
            .find(|d| d.chunk_index == 0)
            .expect("chunk 0");
        assert_eq!(
            chunk0.archive_addr, [0xA2u8; 20],
            "chunk 0 must retry on archive 1"
        );
        let chunk2 = dispatches
            .iter()
            .find(|d| d.chunk_index == 2)
            .expect("chunk 2");
        assert_eq!(
            chunk2.archive_addr, [0xA1u8; 20],
            "chunk 2 untouched, tries archive 0"
        );
    }

    /// Wrong-hash failures take the same code path as peer-error
    /// failures: the event loop clears `in_flight_to` and bumps
    /// `next_attempt_idx`. So the selector behavior is identical to
    /// the failed-archive case — verified explicitly here so a
    /// future split between the two error paths can't silently
    /// break wrong-hash retry routing for the failing chunk only.
    #[test]
    fn select_chunks_to_dispatch_wrong_hash_failure_isolated_to_failing_chunk() {
        let manifest = synth_manifest(4);
        let archives = vec![[0xA1u8; 20], [0xA2u8; 20]];
        let (mut state, addr_to_peer) = synth_state(4, &archives);

        // Chunk 0: archive[0] returned wrong-hash → cleared in-flight,
        // advanced index. Chunks 1, 2: still in-flight on archive[0].
        // Chunk 3: pending.
        state.get_mut(&0).unwrap().next_attempt_idx = 1;
        let p = addr_to_peer[&[0xA1u8; 20]];
        state.get_mut(&1).unwrap().in_flight_to = Some((p, [0xA1u8; 20]));
        state.get_mut(&1).unwrap().next_attempt_idx = 1;
        state.get_mut(&2).unwrap().in_flight_to = Some((p, [0xA1u8; 20]));
        state.get_mut(&2).unwrap().next_attempt_idx = 1;

        // max_concurrent=4: 2 already in-flight → 2 more slots.
        // Chunk 0 retries on archive[1]; chunk 3 picks up archive[0].
        // Chunks 1 and 2 are NOT disturbed.
        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 4);
        assert_eq!(dispatches.len(), 2);
        let chunk0 = dispatches.iter().find(|d| d.chunk_index == 0).unwrap();
        assert_eq!(chunk0.archive_addr, [0xA2u8; 20]);
        // Chunks 1, 2 must NOT appear in dispatches (they're already in-flight).
        assert!(!dispatches.iter().any(|d| d.chunk_index == 1));
        assert!(!dispatches.iter().any(|d| d.chunk_index == 2));
    }

    /// All archives for a chunk exhausted (next_attempt_idx ==
    /// assigned.len()) → selector simply skips the chunk. The event
    /// loop's failure-detection branch is what surfaces the typed
    /// `ChunkFetch` error; the selector's job here is only to
    /// confirm nothing dispatches for that chunk.
    #[test]
    fn select_chunks_to_dispatch_skips_exhausted_chunk() {
        let manifest = synth_manifest(2);
        let archives = vec![[0xA1u8; 20]];
        let (mut state, addr_to_peer) = synth_state(2, &archives);

        // Chunk 0: tried the only archive; exhausted.
        state.get_mut(&0).unwrap().next_attempt_idx = 1;

        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 4);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].chunk_index, 1, "only chunk 1 dispatchable");
    }

    /// `max_concurrent = 0` is clamped to 1 — pin this so a misconfigured
    /// `--max-concurrent 0` never hangs the download forever waiting
    /// for slot 0 to free up.
    #[test]
    fn select_chunks_to_dispatch_zero_clamps_to_one() {
        let manifest = synth_manifest(3);
        let archives = vec![[0xA1u8; 20]];
        let (state, addr_to_peer) = synth_state(3, &archives);

        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 0);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].chunk_index, 0);
    }

    /// Unresolvable archives don't dispatch and don't burn the
    /// archive attempt — the selector waits for a future
    /// PeerIdentified to arrive. Otherwise a temporary
    /// PeerId-resolution gap would skip a perfectly-good archive
    /// and could make a chunk fail that would have succeeded.
    #[test]
    fn select_chunks_to_dispatch_unresolvable_archive_waits_no_burn() {
        let manifest = synth_manifest(2);
        let archives = vec![[0xAAu8; 20]];
        let mut state: HashMap<u32, ChunkFetchState> = (0..2)
            .map(|i| {
                (
                    i,
                    ChunkFetchState {
                        assigned: archives.clone(),
                        next_attempt_idx: 0,
                        in_flight_to: None,
                        received: None,
                    },
                )
            })
            .collect();
        // Empty addr_to_peer → the assigned archive isn't resolvable.
        let addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();

        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 8);
        assert!(
            dispatches.is_empty(),
            "no archives resolvable → no dispatches; got {dispatches:?}"
        );
        // next_attempt_idx must NOT have advanced — selector is
        // pure, doesn't mutate state, but pin the precondition so a
        // future refactor that "advances on probe failure" gets
        // caught here.
        for cd in &manifest.chunks {
            assert_eq!(state.get_mut(&cd.chunk_index).unwrap().next_attempt_idx, 0);
        }
    }

    /// Already-received chunks don't get re-dispatched even when
    /// concurrency slots are open. The completion path is one-way.
    #[test]
    fn select_chunks_to_dispatch_skips_received_chunks() {
        let manifest = synth_manifest(3);
        let archives = vec![[0xA1u8; 20]];
        let (mut state, addr_to_peer) = synth_state(3, &archives);

        // Chunk 1 already received.
        state.get_mut(&1).unwrap().received = Some(vec![0xCC; 64]);

        let dispatches = select_chunks_to_dispatch(&state, &manifest.chunks, &addr_to_peer, 8);
        let indices: Vec<u32> = dispatches.iter().map(|d| d.chunk_index).collect();
        assert_eq!(
            indices,
            vec![0, 2],
            "chunk 1 already done; dispatch 0 and 2"
        );
    }

    // ── Phase 4d manifest fan-out: compute_manifest_fanout ─────────

    /// `compute_manifest_fanout` enforces three invariants:
    ///   * `max_concurrent == 0` clamps to 1 — a misconfigured CLI
    ///     value cannot deadlock the fetch loop.
    ///   * Upper bound is 3 — manifests are tiny and small fan-out is
    ///     enough to mask one slow archive; raising the cap inflates
    ///     inbound bandwidth on archives without buying resilience.
    ///   * `min(assigned_size)` prevents dispatching more requests
    ///     than there are candidates.
    /// `assigned_size == 0` returns `0` (caller bails before entering
    /// the fetch loop).
    #[test]
    fn compute_manifest_fanout_zero_clamps_to_one() {
        assert_eq!(compute_manifest_fanout(0, 5), 1);
    }

    #[test]
    fn compute_manifest_fanout_upper_bound_three() {
        // Operator passes --max-concurrent 100; assigned set has 5
        // archives. Fan-out is capped at 3, not raised toward
        // assigned_size or max_concurrent.
        assert_eq!(compute_manifest_fanout(100, 5), 3);
        assert_eq!(compute_manifest_fanout(4, 5), 3);
        assert_eq!(compute_manifest_fanout(3, 5), 3);
    }

    #[test]
    fn compute_manifest_fanout_below_upper_bound_is_passthrough() {
        // max_concurrent within [1,3] is honored exactly when the
        // assigned set has at least that many archives.
        assert_eq!(compute_manifest_fanout(1, 5), 1);
        assert_eq!(compute_manifest_fanout(2, 5), 2);
    }

    #[test]
    fn compute_manifest_fanout_clamped_to_assigned_size() {
        // 1-archive file: even max_concurrent = 100 produces fan-out
        // = 1 because there's only one candidate to ask.
        assert_eq!(compute_manifest_fanout(100, 1), 1);
        // 2-archive file with max_concurrent = 3: fan-out = 2.
        assert_eq!(compute_manifest_fanout(3, 2), 2);
    }

    #[test]
    fn compute_manifest_fanout_zero_assigned_returns_zero() {
        // Empty assigned set is the caller's signal to bail. Returning
        // 0 (rather than 1) makes the fetch loop's own termination
        // checks short-circuit cleanly.
        assert_eq!(compute_manifest_fanout(3, 0), 0);
        assert_eq!(compute_manifest_fanout(0, 0), 0);
    }

    // ── Phase 4d manifest fan-out: select_manifest_dispatch ────────

    /// Build a synthetic archive_status map (all `Untried`) and a
    /// matching addr_to_peer (every archive resolvable to a fresh
    /// fake PeerId). Mirrors the chunk-concurrency `synth_state`.
    fn synth_manifest_fanout_state(
        archives: &[[u8; 20]],
    ) -> (
        HashMap<[u8; 20], ManifestArchiveStatus>,
        HashMap<[u8; 20], PeerId>,
    ) {
        let archive_status = archives
            .iter()
            .map(|a| (*a, ManifestArchiveStatus::Untried))
            .collect();
        let addr_to_peer = archives
            .iter()
            .map(|addr| {
                (
                    *addr,
                    sum_net::Keypair::generate_ed25519().public().to_peer_id(),
                )
            })
            .collect();
        (archive_status, addr_to_peer)
    }

    /// `fanout = 1` collapses to one in-flight at a time — sequential
    /// fallback identical to the Phase 4b single-peer behavior.
    #[test]
    fn select_manifest_dispatch_fanout_one_is_sequential() {
        let archives: Vec<[u8; 20]> = (1u8..=5).map(|b| [b; 20]).collect();
        let (status, addr_to_peer) = synth_manifest_fanout_state(&archives);
        let dispatches = select_manifest_dispatch(&status, &addr_to_peer, 1);
        assert_eq!(dispatches.len(), 1);
        // Sorted-by-address dispatch order: lowest archive first.
        assert_eq!(dispatches[0].archive_addr, [1u8; 20]);
    }

    /// `fanout = N` produces exactly N dispatches when at least N
    /// untried-and-resolvable archives exist.
    #[test]
    fn select_manifest_dispatch_caps_in_flight_at_fanout() {
        let archives: Vec<[u8; 20]> = (1u8..=5).map(|b| [b; 20]).collect();
        let (status, addr_to_peer) = synth_manifest_fanout_state(&archives);

        for fanout in [1usize, 2, 3, 5] {
            let dispatches = select_manifest_dispatch(&status, &addr_to_peer, fanout);
            assert_eq!(
                dispatches.len(),
                fanout.min(archives.len()),
                "fanout={fanout} produced {} dispatches",
                dispatches.len()
            );
        }
    }

    /// Existing in-flight count is subtracted from the cap. If 2
    /// archives are already Dispatched and fanout = 3, only ONE more
    /// gets selected this wave.
    #[test]
    fn select_manifest_dispatch_respects_existing_in_flight() {
        let archives: Vec<[u8; 20]> = (1u8..=5).map(|b| [b; 20]).collect();
        let (mut status, addr_to_peer) = synth_manifest_fanout_state(&archives);
        status.insert([1u8; 20], ManifestArchiveStatus::Dispatched);
        status.insert([2u8; 20], ManifestArchiveStatus::Dispatched);

        let dispatches = select_manifest_dispatch(&status, &addr_to_peer, 3);
        assert_eq!(
            dispatches.len(),
            1,
            "2 already dispatched, fanout 3 → 1 more"
        );
        // The remaining slot goes to the lowest-address Untried (3).
        assert_eq!(dispatches[0].archive_addr, [3u8; 20]);
    }

    /// `Failed` and `Dispatched` archives are not re-dispatched.
    /// Selector only picks `Untried` candidates.
    #[test]
    fn select_manifest_dispatch_skips_dispatched_and_failed() {
        let archives: Vec<[u8; 20]> = (1u8..=4).map(|b| [b; 20]).collect();
        let (mut status, addr_to_peer) = synth_manifest_fanout_state(&archives);
        status.insert([1u8; 20], ManifestArchiveStatus::Failed);
        status.insert([2u8; 20], ManifestArchiveStatus::Dispatched);
        // 3 and 4 remain Untried.

        let dispatches = select_manifest_dispatch(&status, &addr_to_peer, 3);
        // 1 already in-flight → 2 slots remain. Both Untried picked.
        let picked: Vec<[u8; 20]> = dispatches.iter().map(|d| d.archive_addr).collect();
        assert_eq!(picked, vec![[3u8; 20], [4u8; 20]]);
    }

    /// Archives whose PeerId is not yet in `addr_to_peer` are
    /// SKIPPED (not held). Manifest archives are unordered: passing
    /// over an unresolvable archive in favor of a resolvable one
    /// doesn't violate any priority invariant. Status stays
    /// `Untried` so a future PeerIdentified picks it up.
    #[test]
    fn select_manifest_dispatch_skips_unresolvable_archives() {
        let archives: Vec<[u8; 20]> = (1u8..=4).map(|b| [b; 20]).collect();
        let (status, mut addr_to_peer) = synth_manifest_fanout_state(&archives);
        // Lowest-address archive (1) is unresolvable.
        addr_to_peer.remove(&[1u8; 20]);

        let dispatches = select_manifest_dispatch(&status, &addr_to_peer, 3);
        let picked: Vec<[u8; 20]> = dispatches.iter().map(|d| d.archive_addr).collect();
        // Skipped 1, picked next 3 in sorted order.
        assert_eq!(picked, vec![[2u8; 20], [3u8; 20], [4u8; 20]]);
    }

    /// All archives `Failed`: selector returns empty. The fetch loop
    /// uses this state to detect "exhausted, return typed error".
    #[test]
    fn select_manifest_dispatch_empty_when_all_failed() {
        let archives: Vec<[u8; 20]> = (1u8..=3).map(|b| [b; 20]).collect();
        let (mut status, addr_to_peer) = synth_manifest_fanout_state(&archives);
        for a in &archives {
            status.insert(*a, ManifestArchiveStatus::Failed);
        }
        let dispatches = select_manifest_dispatch(&status, &addr_to_peer, 3);
        assert!(dispatches.is_empty());
    }

    /// No resolvable Untried archives: selector returns empty even
    /// though Untried entries exist. The fetch loop keeps waiting on
    /// PeerIdentified events until the deadline.
    #[test]
    fn select_manifest_dispatch_empty_when_no_resolvable_untried() {
        let archives: Vec<[u8; 20]> = (1u8..=3).map(|b| [b; 20]).collect();
        let (status, _) = synth_manifest_fanout_state(&archives);
        // Empty addr_to_peer = no archive resolvable.
        let empty_addr_to_peer: HashMap<[u8; 20], PeerId> = HashMap::new();
        let dispatches = select_manifest_dispatch(&status, &empty_addr_to_peer, 3);
        assert!(dispatches.is_empty());
    }

    /// Sort-by-address determinism: given the same inputs, the
    /// selector returns the same dispatch list every call. This is
    /// the load-bearing testability property — concurrency invariants
    /// tested here mirror production behavior 1:1.
    #[test]
    fn select_manifest_dispatch_is_deterministic() {
        let archives: Vec<[u8; 20]> = vec![[0x42; 20], [0x10; 20], [0xAA; 20], [0x80; 20]];
        let (status, addr_to_peer) = synth_manifest_fanout_state(&archives);
        let first = select_manifest_dispatch(&status, &addr_to_peer, 3);
        let second = select_manifest_dispatch(&status, &addr_to_peer, 3);
        assert_eq!(first, second);
        // First-by-address is 0x10.
        assert_eq!(first[0].archive_addr, [0x10u8; 20]);
        // Then 0x42, 0x80.
        assert_eq!(first[1].archive_addr, [0x42u8; 20]);
        assert_eq!(first[2].archive_addr, [0x80u8; 20]);
    }
}
