//! Phase 4c — owner-side access-list mutations for Private V2 files.
//!
//! Three operator-facing operations:
//!
//!   * `run_share`   → submit `AddAccessV2`. Owner recovers `K_file`
//!                     locally from their own access bundle, wraps it
//!                     for the new recipient's registered X25519 key,
//!                     and submits the new entry on chain. The chain
//!                     never sees `K_file`.
//!   * `run_revoke`  → submit `RemoveAccessV2`. Chain-side ACL denies
//!                     the address immediately on the next pull. We
//!                     do NOT rotate `K_file`; forward secrecy
//!                     (re-keying the file under a fresh `K_file`)
//!                     is Phase 5+. Operators who need it should
//!                     revoke + re-ingest.
//!   * `run_update_access` → submit `UpdateAccessV2`, today supports
//!                     setting/clearing the entry's `expires_at`. The
//!                     existing `encrypted_key_bundle` is preserved
//!                     verbatim; passing a different bundle would
//!                     silently break the recipient's downloads.
//!
//! All three are V2-only. Public files and V1/legacy files are
//! refused up front with a typed error — these are V2 chain ops and
//! the V2 access semantics don't apply to V1.
//!
//! ## Privacy invariants
//!
//! * `K_file` is recovered ONLY from the owner's own access bundle on
//!   chain — never asked from the chain or from peers.
//! * Recovered `K_file` lives in `Zeroizing<[u8; 32]>` for its full
//!   lifetime in this module.
//! * The chain's view of every mutation is identical to a normal
//!   `AccessEntryV2` add/remove/update — only encrypted bundles cross
//!   the wire. Archives still see ciphertext only.
//!
//! ## Pre-flight policy (privacy-first; no fee burned on rejected txs)
//!
//! Each command does its full set of pre-flight RPC checks BEFORE any
//! signing or `send_raw_transaction`. The order is:
//!
//!   1. V2-enabled gate (mirrors the ingest gate).
//!   2. `storage_getFileInfoV2` confirms Private V2.
//!   3. Operator owns the file.
//!   4. Recipient/target preconditions specific to each op.
//!   5. Only then: derive K_file (share only), build tx, submit, wait.

use std::sync::Arc;

use anyhow::{Context, Result};
use thiserror::Error;
use tracing::{info, warn};
use zeroize::Zeroizing;

use sum_crypto::{
    RECIPIENT_BUNDLE_SIZE, unwrap_for_self, wrap_for_recipient, x25519_keypair_from_ed25519_seed,
};
use sum_net::{Keypair, identity};
use sum_types::rpc_types::{AccessEntryV2, StorageFileInfoV2};

use crate::rpc_client::L1RpcClient;
use crate::tx_builder::{
    AccessEntryV2Mirror, Bundle80, build_add_access_v2_tx, build_remove_access_v2_tx,
    build_update_access_v2_tx,
};
use crate::tx_wait::{DEFAULT_POLL_INTERVAL, TxWaitError, wait_for_finalized};

/// Pagination chunk size for access-list scans. Matches the chain's
/// default per-page limit.
const ACCESS_PAGE_SIZE: u32 = 256;
/// Maximum pages we'll scan before giving up. 64 × 256 = 16,384
/// entries — well past any realistic file's access list.
const ACCESS_MAX_PAGES: u32 = 64;
/// Wait window for tx finality. Matches the existing
/// `RegisterEncryptionKey` runner; access mutations are similarly
/// chain-only and don't need a longer wait.
const FINALITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum AccessOpError {
    #[error(
        "file is not Private V2 (visibility={visibility}, lifecycle={lifecycle}); access mutations apply only to V2 Private files"
    )]
    NotV2Private { visibility: u8, lifecycle: u8 },

    #[error(
        "file is Private V2 but lifecycle is {lifecycle} (Pending=0, Active=1, Abandoned=2); \
         access mutations are Active-only — chain rejects them in any other state. \
         Wait for ActivateFileV2 to finalize, or for an Abandoned file submit nothing — \
         it is terminal."
    )]
    NotActive { lifecycle: u8 },

    #[error("file is V1 / legacy or not registered V2; this command targets only V2 Private files")]
    NotV2 {
        #[source]
        source: anyhow::Error,
    },

    #[error("operator address {operator_b58} is not the file owner ({owner_b58})")]
    NotOwner {
        operator_b58: String,
        owner_b58: String,
    },

    #[error("address {addr_b58} is already in the file's access list")]
    AlreadyInAccessList { addr_b58: String },

    #[error("address {addr_b58} is not in the file's access list")]
    NotInAccessList { addr_b58: String },

    #[error(
        "refusing to revoke the file owner's own access ({addr_b58}) — it would brick the \
         operator's ability to recover K_file for future shares"
    )]
    OwnerSelfRevoke { addr_b58: String },

    #[error(
        "recipient {addr_b58} has no encryption pubkey on chain; ask them to run \
         `sum-node register-encryption-key` first"
    )]
    RecipientHasNoEncryptionKey { addr_b58: String },

    #[error(
        "owner ({addr_b58}) is missing from their own file's access list — chain rule violation; \
         refusing to proceed"
    )]
    OwnerEntryMissing { addr_b58: String },

    #[error(
        "owner's access entry exists but carries no encrypted_key_bundle — chain rule violation; \
         refusing to proceed"
    )]
    OwnerBundleMissing,

    #[error(
        "access bundle wire shape invalid (expected {RECIPIENT_BUNDLE_SIZE}-byte hex): {reason}"
    )]
    BundleHex { reason: String },

    #[error("failed to unwrap K_file from owner bundle: {0}")]
    BundleUnwrap(#[source] sum_crypto::CryptoError),

    #[error("failed to wrap K_file for recipient: {0}")]
    BundleWrap(#[source] sum_crypto::CryptoError),

    #[error(
        "update-access requires an explicit expiry directive (`<addr>:<height>` to set, \
         `<addr>:none` to clear); a bare `<addr>` would be a no-op"
    )]
    UpdateRequiresExpiryDirective,

    #[error(
        "update-access target's expiry already matches the requested value ({:?}) — refusing no-op",
        requested
    )]
    UpdateNoOp { requested: Option<u64> },

    #[error("V2 not enabled on this chain or not yet active at finalized height")]
    V2NotEnabled {
        #[source]
        source: anyhow::Error,
    },

    #[error("RPC error: {0}")]
    Rpc(#[source] anyhow::Error),

    #[error("tx submission failed: {0}")]
    TxSubmit(#[source] anyhow::Error),

    #[error("tx finality wait failed: {0}")]
    TxWait(#[from] TxWaitError),
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Recipient spec parsed from the CLI form `<addr_b58>[:<height>|:none]`.
/// `share` accepts all three flavors (no expiry / explicit height /
/// explicit `none` for clarity); `revoke` ignores the expiry portion;
/// `update-access` requires an explicit `:` directive.
#[derive(Debug, Clone)]
pub struct RecipientSpec {
    pub addr: [u8; 20],
    pub addr_b58: String,
    pub expiry: ExpiryDirective,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryDirective {
    /// No `:` segment supplied — leave expiry untouched (`None`).
    Unset,
    /// `:none` — explicitly clear an existing expiry.
    Clear,
    /// `:<height>` — set/replace expiry to the given finalized height.
    SetTo(u64),
}

impl ExpiryDirective {
    /// Resolve to the `Option<u64>` that goes onto the chain entry.
    /// Both `Unset` and `Clear` map to `None`; `SetTo(h)` maps to
    /// `Some(h)`. The distinction matters at the parser layer (where
    /// `update-access` requires explicit form) but not on the wire.
    pub fn to_chain_value(self) -> Option<u64> {
        match self {
            ExpiryDirective::Unset | ExpiryDirective::Clear => None,
            ExpiryDirective::SetTo(h) => Some(h),
        }
    }
}

pub fn parse_recipient_spec(s: &str) -> Result<RecipientSpec, AccessOpError> {
    let (addr_str, expiry) = match s.split_once(':') {
        None => (s, ExpiryDirective::Unset),
        Some((a, "none")) | Some((a, "None")) | Some((a, "NONE")) => (a, ExpiryDirective::Clear),
        Some((a, rest)) => {
            let h: u64 = rest.parse().map_err(|_| AccessOpError::BundleHex {
                reason: format!("expiry segment {rest:?} is neither `none` nor a u64 block height"),
            })?;
            (a, ExpiryDirective::SetTo(h))
        }
    };
    if addr_str.is_empty() {
        return Err(AccessOpError::BundleHex {
            reason: "empty L1 address".into(),
        });
    }
    let addr =
        identity::l1_address_from_base58(addr_str).map_err(|e| AccessOpError::BundleHex {
            reason: format!("bad base58: {e}"),
        })?;
    let addr_b58 = identity::l1_address_base58(&addr);
    Ok(RecipientSpec {
        addr,
        addr_b58,
        expiry,
    })
}

/// Add a new recipient to a Private V2 file's access list. See module
/// doc for the full pre-flight + recovery + wrap pipeline.
pub async fn run_share(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id: u64,
    fee: u128,
    merkle_root: [u8; 32],
    recipient: RecipientSpec,
) -> Result<()> {
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
    let operator_addr = identity::l1_address_from_keypair(&keypair);
    let operator_b58 = identity::l1_address_base58(&operator_addr);
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    info!(
        root = %root_hex,
        operator = %operator_b58,
        recipient = %recipient.addr_b58,
        "share: starting"
    );

    // 1. V2-enabled + finalized gate.
    require_v2_enabled(rpc.as_ref()).await?;

    // 2. File is V2 Private + operator owns it.
    let info = require_private_v2_owner(rpc.as_ref(), &root_hex, &operator_b58).await?;

    // 3. Recipient pre-checks.
    if let Some(_existing) =
        find_access_entry(rpc.as_ref(), &root_hex, &recipient.addr_b58, &info).await?
    {
        return Err(AccessOpError::AlreadyInAccessList {
            addr_b58: recipient.addr_b58,
        }
        .into());
    }
    let recipient_pk = rpc
        .account_get_encryption_public_key(&recipient.addr_b58)
        .await
        .map_err(AccessOpError::Rpc)?
        .ok_or_else(|| AccessOpError::RecipientHasNoEncryptionKey {
            addr_b58: recipient.addr_b58.clone(),
        })?;

    // 4. K_file recovery (client-side only).
    let k_file = recover_k_file_from_owner_bundle(
        rpc.as_ref(),
        &seed,
        operator_addr,
        &operator_b58,
        &info,
        &root_hex,
    )
    .await?;

    // 5. Wrap K_file for the new recipient.
    let bundle = wrap_for_recipient(&k_file, &recipient.addr, &recipient_pk)
        .map_err(AccessOpError::BundleWrap)?;

    // 6. Build + submit tx.
    let entry = AccessEntryV2Mirror {
        address: recipient.addr,
        encrypted_key_bundle: Some(Bundle80(bundle)),
        expires_at: recipient.expiry.to_chain_value(),
    };
    let tx_hash = submit_and_wait(
        rpc.as_ref(),
        &operator_b58,
        chain_id,
        fee,
        &seed,
        |nonce| build_add_access_v2_tx(&seed, chain_id, nonce, fee, merkle_root, entry.clone()),
        "AddAccessV2",
    )
    .await?;

    info!(
        root = %root_hex,
        recipient = %recipient.addr_b58,
        %tx_hash,
        expires_at = ?recipient.expiry.to_chain_value(),
        "share: AddAccessV2 finalized — recipient can download once the AddAccessV2 tx is finalized and their node uses the matching key"
    );
    Ok(())
}

/// Remove a recipient from a Private V2 file's access list. The
/// recipient's chain-side access denies immediately at finalization.
/// Phase 4c does not rotate K_file; forward secrecy on revocation is
/// Phase 5+.
pub async fn run_revoke(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id: u64,
    fee: u128,
    merkle_root: [u8; 32],
    target: RecipientSpec,
) -> Result<()> {
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
    let operator_addr = identity::l1_address_from_keypair(&keypair);
    let operator_b58 = identity::l1_address_base58(&operator_addr);
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    info!(
        root = %root_hex,
        operator = %operator_b58,
        target = %target.addr_b58,
        "revoke: starting"
    );

    require_v2_enabled(rpc.as_ref()).await?;
    let info = require_private_v2_owner(rpc.as_ref(), &root_hex, &operator_b58).await?;

    if target.addr == operator_addr {
        return Err(AccessOpError::OwnerSelfRevoke {
            addr_b58: target.addr_b58,
        }
        .into());
    }
    if find_access_entry(rpc.as_ref(), &root_hex, &target.addr_b58, &info)
        .await?
        .is_none()
    {
        return Err(AccessOpError::NotInAccessList {
            addr_b58: target.addr_b58,
        }
        .into());
    }

    let tx_hash = submit_and_wait(
        rpc.as_ref(),
        &operator_b58,
        chain_id,
        fee,
        &seed,
        |nonce| build_remove_access_v2_tx(&seed, chain_id, nonce, fee, merkle_root, target.addr),
        "RemoveAccessV2",
    )
    .await?;

    info!(
        root = %root_hex,
        target = %target.addr_b58,
        %tx_hash,
        "revoke: RemoveAccessV2 finalized — chain-side ACL denies the target on the next pull. \
         NOTE: K_file is not rotated; the revoked recipient still holds their old bundle locally. \
         If forward secrecy matters, revoke + re-ingest the file under a fresh K_file."
    );
    Ok(())
}

/// Update an existing recipient's `expires_at`. First-cut Phase 4c
/// supports set-to-height (`<addr>:<height>`) and clear
/// (`<addr>:none`); a bare `<addr>` is rejected as a no-op so the
/// operator's intent is unambiguous.
pub async fn run_update_access(
    keypair: Keypair,
    seed: [u8; 32],
    rpc_url: String,
    chain_id: u64,
    fee: u128,
    merkle_root: [u8; 32],
    target: RecipientSpec,
) -> Result<()> {
    let rpc = Arc::new(L1RpcClient::new(rpc_url));
    let operator_addr = identity::l1_address_from_keypair(&keypair);
    let operator_b58 = identity::l1_address_base58(&operator_addr);
    let root_hex = format!("0x{}", hex::encode(merkle_root));

    if matches!(target.expiry, ExpiryDirective::Unset) {
        return Err(AccessOpError::UpdateRequiresExpiryDirective.into());
    }
    let new_expires_at = target.expiry.to_chain_value();

    info!(
        root = %root_hex,
        operator = %operator_b58,
        target = %target.addr_b58,
        new_expires_at = ?new_expires_at,
        "update-access: starting"
    );

    require_v2_enabled(rpc.as_ref()).await?;
    let info = require_private_v2_owner(rpc.as_ref(), &root_hex, &operator_b58).await?;

    let existing = find_access_entry(rpc.as_ref(), &root_hex, &target.addr_b58, &info)
        .await?
        .ok_or_else(|| AccessOpError::NotInAccessList {
            addr_b58: target.addr_b58.clone(),
        })?;
    if existing.expires_at == new_expires_at {
        return Err(AccessOpError::UpdateNoOp {
            requested: new_expires_at,
        }
        .into());
    }

    // Preserve the existing encrypted bundle byte-for-byte. Re-wrapping
    // here would either need the operator's K_file (unnecessary for an
    // expiry change) or silently change the recipient's bundle (bad).
    let existing_bundle_hex = existing
        .encrypted_key_bundle
        .as_deref()
        .ok_or(AccessOpError::OwnerBundleMissing)?;
    let existing_bundle = parse_bundle_hex(existing_bundle_hex)?;

    let new_entry = AccessEntryV2Mirror {
        address: target.addr,
        encrypted_key_bundle: Some(Bundle80(existing_bundle)),
        expires_at: new_expires_at,
    };

    let tx_hash = submit_and_wait(
        rpc.as_ref(),
        &operator_b58,
        chain_id,
        fee,
        &seed,
        |nonce| {
            build_update_access_v2_tx(
                &seed,
                chain_id,
                nonce,
                fee,
                merkle_root,
                target.addr,
                new_entry.clone(),
            )
        },
        "UpdateAccessV2",
    )
    .await?;

    info!(
        root = %root_hex,
        target = %target.addr_b58,
        old_expires_at = ?existing.expires_at,
        new_expires_at = ?new_expires_at,
        %tx_hash,
        "update-access: UpdateAccessV2 finalized"
    );
    Ok(())
}

// ── Crate-internal helpers ───────────────────────────────────────────────────

/// V2-enabled gate. Matches the gate used by ingest / abandon.
async fn require_v2_enabled(rpc: &L1RpcClient) -> Result<(), AccessOpError> {
    let cp = rpc
        .chain_get_chain_params()
        .await
        .map_err(|e| AccessOpError::V2NotEnabled { source: e })?;
    let head = rpc
        .chain_get_block_height()
        .await
        .map_err(|e| AccessOpError::V2NotEnabled { source: e })?;
    match cp.v2_enabled_from_height {
        None => Err(AccessOpError::V2NotEnabled {
            source: anyhow::anyhow!("chain v2_enabled_from_height = null"),
        }),
        Some(h) if head.height < h => Err(AccessOpError::V2NotEnabled {
            source: anyhow::anyhow!(
                "finalized height {} < v2_enabled_from_height {h} ({} blocks remaining)",
                head.height,
                h - head.height
            ),
        }),
        Some(_) => Ok(()),
    }
}

/// Probe the V2 row, refuse if Public / V1 / unavailable, refuse if
/// `lifecycle != Active`, and refuse if the operator doesn't own the
/// file. Returns the V2 row on success so callers can reuse
/// `info.access_list` for subsequent pre-flight checks without a
/// second RPC.
///
/// Lifecycle gate: chain plan §3.1 makes
/// `AddAccessV2`/`RemoveAccessV2`/`UpdateAccessV2` valid only when
/// the file is Active. Pending files are still in S2/S3/S4 and the
/// access list is finalized by `ActivateFileV2`; Abandoned files are
/// terminal. We refuse here BEFORE signing or submitting so the
/// operator doesn't burn the fee on a chain that will reject the tx
/// anyway. Order is: V2 row → Private → Active → owner. Owner check
/// is last because revealing "you're not the owner" before "this
/// file is the wrong shape" would leak owner identity to anyone who
/// can probe.
async fn require_private_v2_owner(
    rpc: &L1RpcClient,
    root_hex: &str,
    operator_b58: &str,
) -> Result<StorageFileInfoV2, AccessOpError> {
    let info = match rpc
        .storage_get_file_info_v2(root_hex, Some(0), Some(ACCESS_PAGE_SIZE))
        .await
    {
        Ok(Some(i)) => i,
        // Clean not-found: the chain has no V2 row for this root. This is
        // the "not registered as V2" reason — distinct from a transport
        // failure below.
        Ok(None) => {
            return Err(AccessOpError::NotV2 {
                source: anyhow::anyhow!(
                    "storage_getFileInfoV2 returned no row for {root_hex} (not registered as V2)"
                ),
            });
        }
        // Transport / RPC failure — surface as a transport error, not as
        // a "not V2" verdict (the lookup never completed).
        Err(e) => {
            return Err(AccessOpError::Rpc(e));
        }
    };
    if !info.visibility.is_private() {
        return Err(AccessOpError::NotV2Private {
            visibility: info.visibility.0,
            lifecycle: info.lifecycle.0,
        });
    }
    if !info.lifecycle.is_active() {
        return Err(AccessOpError::NotActive {
            lifecycle: info.lifecycle.0,
        });
    }
    if info.owner != operator_b58 {
        return Err(AccessOpError::NotOwner {
            operator_b58: operator_b58.to_string(),
            owner_b58: info.owner.clone(),
        });
    }
    Ok(info)
}

/// Paginated access-list scan. The seed page is reused (callers
/// usually already have it from `require_private_v2_owner`); we only
/// page if the seed page was full AND didn't contain the target.
async fn find_access_entry(
    rpc: &L1RpcClient,
    root_hex: &str,
    target_b58: &str,
    seed_page: &StorageFileInfoV2,
) -> Result<Option<AccessEntryV2>, AccessOpError> {
    if let Some(e) = seed_page
        .access_list
        .iter()
        .find(|e| e.address == target_b58)
    {
        return Ok(Some(e.clone()));
    }
    if seed_page.access_list.len() < ACCESS_PAGE_SIZE as usize {
        return Ok(None);
    }
    for page_idx in 1..ACCESS_MAX_PAGES {
        let offset = page_idx * ACCESS_PAGE_SIZE;
        let page = match rpc
            .storage_get_file_info_v2(root_hex, Some(offset), Some(ACCESS_PAGE_SIZE))
            .await
        {
            Ok(Some(page)) => page,
            // A later page returned null — the V2 row is gone mid-scan.
            // Terminal: the target simply isn't in the access list.
            Ok(None) => return Ok(None),
            Err(e) => return Err(AccessOpError::Rpc(e)),
        };
        if let Some(e) = page.access_list.iter().find(|e| e.address == target_b58) {
            return Ok(Some(e.clone()));
        }
        if page.access_list.len() < ACCESS_PAGE_SIZE as usize {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Recover the file's `K_file` from the owner's own access bundle on
/// chain. Used by `share` (and slated for reuse by Phase 4d's Private
/// resume). Lives here so the privacy-critical flow stays in one
/// place — the unwrap output is always `Zeroizing<[u8; 32]>`.
pub(crate) async fn recover_k_file_from_owner_bundle(
    rpc: &L1RpcClient,
    seed: &[u8; 32],
    owner_addr: [u8; 20],
    owner_b58: &str,
    seed_page: &StorageFileInfoV2,
    root_hex: &str,
) -> Result<Zeroizing<[u8; 32]>, AccessOpError> {
    let entry = find_access_entry(rpc, root_hex, owner_b58, seed_page)
        .await?
        .ok_or_else(|| AccessOpError::OwnerEntryMissing {
            addr_b58: owner_b58.to_string(),
        })?;
    unwrap_owner_entry(seed, owner_addr, &entry)
}

/// Synchronous variant of [`recover_k_file_from_owner_bundle`]: uses
/// ONLY the supplied seed page, no pagination.
///
/// **Why no pagination is fine for resume.** The chain caps the
/// total `access_list` byte size (chain plan §3.1
/// `max_access_list_bytes`), and each `AccessEntryV2` carries an
/// 80-byte `Bundle80`, a base58 address, and an `Option<u64>`
/// expiry. The default first page (256 entries) comfortably absorbs
/// any realistic Private file's access list under that cap, and
/// Phase 4a's "owner-first" insertion order puts the owner at index
/// 0 of the access list. So for resume — which only needs the
/// owner's entry to recover `K_file` — a single-page lookup is
/// sufficient.
///
/// If a future chain rev raises the access-list cap such that
/// real-world files need more than one page, resume must switch to
/// the paginated [`recover_k_file_from_owner_bundle`] (which already
/// exists for `share`'s use case) — the typed `OwnerEntryMissing`
/// error from this helper would surface in that scenario as a clear
/// signal to do so.
///
/// Factored out from `recover_k_file_from_owner_bundle` so callers
/// without a concrete `L1RpcClient` (e.g. `IngestPipeline::resume`,
/// which holds a generic `Arc<R: V2IngestRpc>` so tests can mock the
/// RPC) can still reuse the privacy-critical code path. Refuses with
/// the same typed errors when the bundle is missing or the unwrap
/// fails.
pub(crate) fn recover_k_file_from_seed_page(
    seed: &[u8; 32],
    owner_addr: [u8; 20],
    owner_b58: &str,
    seed_page: &StorageFileInfoV2,
) -> Result<Zeroizing<[u8; 32]>, AccessOpError> {
    let entry = seed_page
        .access_list
        .iter()
        .find(|e| e.address == owner_b58)
        .cloned()
        .ok_or_else(|| AccessOpError::OwnerEntryMissing {
            addr_b58: owner_b58.to_string(),
        })?;
    unwrap_owner_entry(seed, owner_addr, &entry)
}

/// Inner helper shared by both recovery entry points. Parses the
/// bundle hex, derives the X25519 secret, unwraps `K_file`. Held
/// here in one place so the recovery semantics are identical
/// regardless of whether the entry came from a seed page or a
/// paginated walk.
fn unwrap_owner_entry(
    seed: &[u8; 32],
    owner_addr: [u8; 20],
    entry: &AccessEntryV2,
) -> Result<Zeroizing<[u8; 32]>, AccessOpError> {
    let bundle_hex = entry
        .encrypted_key_bundle
        .as_deref()
        .ok_or(AccessOpError::OwnerBundleMissing)?;
    let bundle = parse_bundle_hex(bundle_hex)?;

    // Derive the owner's X25519 secret deterministically from the
    // Ed25519 seed (Phase 4a HKDF: domain
    // `snip-x25519-encryption-key-v1`). Held in zeroizing memory.
    let (x25519_sk_bytes, _x25519_pk) = x25519_keypair_from_ed25519_seed(seed);
    let x25519_sk: Zeroizing<[u8; 32]> = Zeroizing::new(x25519_sk_bytes);

    let k_file_bytes =
        unwrap_for_self(&bundle, &x25519_sk, &owner_addr).map_err(AccessOpError::BundleUnwrap)?;
    Ok(Zeroizing::new(k_file_bytes))
}

/// Parse the on-wire bundle hex into a fixed `[u8; 80]`. Tolerates
/// `0x`-prefixed and bare; case-insensitive (chain commits to
/// lowercase, but legacy / non-canonical responders may differ).
pub(crate) fn parse_bundle_hex(s: &str) -> Result<[u8; RECIPIENT_BUNDLE_SIZE], AccessOpError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).map_err(|e| AccessOpError::BundleHex {
        reason: format!("not valid hex: {e}"),
    })?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| AccessOpError::BundleHex {
            reason: format!(
                "expected {RECIPIENT_BUNDLE_SIZE} bytes, got {}",
                bytes.len()
            ),
        })
}

/// Common send-and-wait wrapper: get nonce, build tx via the supplied
/// closure (so each op only owns its build_*_tx invocation), submit,
/// wait for finality. Returns the tx hash on success.
async fn submit_and_wait<F>(
    rpc: &L1RpcClient,
    operator_b58: &str,
    _chain_id: u64,
    _fee: u128,
    _seed: &[u8; 32],
    build_tx: F,
    label: &'static str,
) -> Result<String, AccessOpError>
where
    F: FnOnce(u64) -> Result<String>,
{
    let nonce = rpc
        .get_nonce(operator_b58)
        .await
        .map_err(AccessOpError::Rpc)?;
    let tx_hex = build_tx(nonce)
        .context("tx build failed")
        .map_err(AccessOpError::TxSubmit)?;
    let tx_hash = rpc
        .send_raw_transaction(&tx_hex)
        .await
        .map_err(AccessOpError::TxSubmit)?;

    info!(label, %tx_hash, "submitted, waiting for finality");
    let height = wait_for_finalized(rpc, &tx_hash, DEFAULT_POLL_INTERVAL, FINALITY_TIMEOUT)
        .await
        .map_err(|e| match e {
            TxWaitError::Failed {
                reason,
                block_height,
            } => {
                warn!(label, ?block_height, %reason, "tx failed at finality");
                AccessOpError::TxWait(TxWaitError::Failed {
                    reason,
                    block_height,
                })
            }
            other => AccessOpError::TxWait(other),
        })?;
    info!(label, %tx_hash, height, "finalized");
    Ok(tx_hash)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recipient_spec_addr_only_means_unset() {
        // Use a valid base58 by constructing one from a known address.
        let addr = [0xAB; 20];
        let s = identity::l1_address_base58(&addr);
        let parsed = parse_recipient_spec(&s).unwrap();
        assert_eq!(parsed.addr, addr);
        assert!(matches!(parsed.expiry, ExpiryDirective::Unset));
        assert_eq!(parsed.expiry.to_chain_value(), None);
    }

    #[test]
    fn parse_recipient_spec_set_height() {
        let addr = [0xBC; 20];
        let s = format!("{}:1234", identity::l1_address_base58(&addr));
        let parsed = parse_recipient_spec(&s).unwrap();
        assert_eq!(parsed.addr, addr);
        assert!(matches!(parsed.expiry, ExpiryDirective::SetTo(1234)));
        assert_eq!(parsed.expiry.to_chain_value(), Some(1234));
    }

    #[test]
    fn parse_recipient_spec_clear_via_none() {
        let addr = [0xCD; 20];
        let b58 = identity::l1_address_base58(&addr);
        for tail in ["none", "None", "NONE"] {
            let s = format!("{b58}:{tail}");
            let parsed = parse_recipient_spec(&s).unwrap();
            assert_eq!(parsed.addr, addr);
            assert!(
                matches!(parsed.expiry, ExpiryDirective::Clear),
                "{tail}: expected Clear"
            );
            assert_eq!(parsed.expiry.to_chain_value(), None);
        }
    }

    #[test]
    fn parse_recipient_spec_rejects_bad_expiry_segment() {
        let addr = [0xDE; 20];
        let b58 = identity::l1_address_base58(&addr);
        let s = format!("{b58}:not-a-number");
        let err = parse_recipient_spec(&s).unwrap_err();
        assert!(matches!(err, AccessOpError::BundleHex { .. }));
    }

    #[test]
    fn parse_recipient_spec_rejects_empty_addr() {
        let err = parse_recipient_spec(":1000").unwrap_err();
        assert!(matches!(err, AccessOpError::BundleHex { .. }));
    }

    #[test]
    fn expiry_directive_to_chain_value_distinct_only_at_parser_layer() {
        // The wire shape collapses Unset and Clear to None; the
        // parser-layer distinction is intentional so update-access can
        // refuse a bare `<addr>` while still allowing `<addr>:none`.
        assert_eq!(ExpiryDirective::Unset.to_chain_value(), None);
        assert_eq!(ExpiryDirective::Clear.to_chain_value(), None);
        assert_eq!(ExpiryDirective::SetTo(42).to_chain_value(), Some(42));
    }

    // ── K_file recovery (no RPC) ─────────────────────────────────────

    /// Cryptographic round-trip of the K_file recovery primitive,
    /// independent of any chain RPC. Build a synthetic
    /// StorageFileInfoV2 whose owner entry carries an
    /// `encrypted_key_bundle` wrapped under the operator's derived
    /// X25519 public key, then call the recovery helper and verify
    /// the unwrapped bytes match the original K_file.
    ///
    /// This is the load-bearing primitive that lets `share` produce a
    /// fresh bundle for a new recipient without ever asking the chain
    /// for `K_file`.
    #[tokio::test]
    async fn recover_k_file_from_owner_bundle_round_trip() {
        use sum_crypto::{wrap_for_recipient, x25519_keypair_from_ed25519_seed};
        use sum_types::rpc_types::{LifecycleV2, VisibilityV2};

        // 1. Owner identity.
        let owner_seed = [0xA1u8; 32];
        let (_x25519_sk, owner_x25519_pk) = x25519_keypair_from_ed25519_seed(&owner_seed);
        let owner_addr = [0x33u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);

        // 2. Random K_file the owner used at ingest time.
        let k_file_plain = [0xFAu8; 32];

        // 3. The owner's access bundle on chain (Phase 4a auto-adds
        //    the owner with a bundle wrapped against their own
        //    X25519 key).
        let bundle = wrap_for_recipient(&k_file_plain, &owner_addr, &owner_x25519_pk).unwrap();
        let bundle_hex = format!("0x{}", hex::encode(bundle));

        // 4. Synthetic V2 row with just the owner entry. `share`
        //    pre-flight uses this as the "seed page".
        let info = StorageFileInfoV2 {
            merkle_root: "0x".to_string() + &"42".repeat(32),
            owner: owner_b58.clone(),
            plaintext_size_bytes: 0,
            stored_size_bytes: 0,
            chunk_count: 0,
            fee_pool: 0,
            created_at: 0,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: 0,
            visibility: VisibilityV2::PRIVATE,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: vec![AccessEntryV2 {
                address: owner_b58.clone(),
                encrypted_key_bundle: Some(bundle_hex),
                expires_at: None,
            }],
        };

        // 5. Call the recovery helper. The RPC arg is unused here
        //    because the seed page already contains the owner entry,
        //    so no pagination request fires; we pass a dummy URL.
        let rpc = L1RpcClient::new("http://127.0.0.1:0".into());
        let recovered = recover_k_file_from_owner_bundle(
            &rpc,
            &owner_seed,
            owner_addr,
            &owner_b58,
            &info,
            "0xroot",
        )
        .await
        .expect("recover K_file");

        // 6. Bytes must match. Zeroizing<[u8; 32]> derefs to [u8; 32].
        assert_eq!(*recovered, k_file_plain);
    }

    /// Owner is missing from their own access list — chain rule
    /// violation. Recovery refuses with a specific typed error so
    /// the operator gets a precise diagnostic instead of a generic
    /// AEAD failure.
    #[tokio::test]
    async fn recover_k_file_refuses_when_owner_entry_missing() {
        use sum_types::rpc_types::{LifecycleV2, VisibilityV2};

        let owner_addr = [0x77u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);
        // Access list contains a different address, NOT the owner.
        let info = StorageFileInfoV2 {
            merkle_root: "0x".to_string() + &"42".repeat(32),
            owner: owner_b58.clone(),
            plaintext_size_bytes: 0,
            stored_size_bytes: 0,
            chunk_count: 0,
            fee_pool: 0,
            created_at: 0,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: 0,
            visibility: VisibilityV2::PRIVATE,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: vec![AccessEntryV2 {
                address: identity::l1_address_base58(&[0x99u8; 20]),
                encrypted_key_bundle: Some(format!("0x{}", "AB".repeat(80))),
                expires_at: None,
            }],
        };
        let rpc = L1RpcClient::new("http://127.0.0.1:0".into());
        let err = recover_k_file_from_owner_bundle(
            &rpc, &[0u8; 32], owner_addr, &owner_b58, &info, "0xroot",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AccessOpError::OwnerEntryMissing { .. }));
    }

    /// Owner entry is present but lacks an `encrypted_key_bundle`
    /// (chain rule violation: Private files must always carry a
    /// bundle for the owner). Refuse with the typed error instead of
    /// trying to unwrap an empty bundle.
    #[tokio::test]
    async fn recover_k_file_refuses_when_owner_bundle_missing() {
        use sum_types::rpc_types::{LifecycleV2, VisibilityV2};

        let owner_addr = [0x88u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);
        let info = StorageFileInfoV2 {
            merkle_root: "0x".to_string() + &"42".repeat(32),
            owner: owner_b58.clone(),
            plaintext_size_bytes: 0,
            stored_size_bytes: 0,
            chunk_count: 0,
            fee_pool: 0,
            created_at: 0,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: 0,
            visibility: VisibilityV2::PRIVATE,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: vec![AccessEntryV2 {
                address: owner_b58.clone(),
                encrypted_key_bundle: None,
                expires_at: None,
            }],
        };
        let rpc = L1RpcClient::new("http://127.0.0.1:0".into());
        let err = recover_k_file_from_owner_bundle(
            &rpc, &[0u8; 32], owner_addr, &owner_b58, &info, "0xroot",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AccessOpError::OwnerBundleMissing));
    }

    // ── Lifecycle gate (Phase 4c-FIX) ──────────────────────────────

    /// Spawn a one-shot HTTP responder that serves the supplied JSON
    /// body to the next inbound connection. Reused across the
    /// lifecycle-gate tests below; matches the helper pattern in
    /// `acl::tests`.
    async fn one_shot_rpc(body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        tokio::spawn(async move {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        url
    }

    /// JSON wire body for a `storage_getFileInfoV2` response with the
    /// requested lifecycle byte. Visibility is Private, owner matches
    /// the test's operator. The `result` is wrapped as a JSON-RPC
    /// 2.0 envelope.
    fn private_v2_info_with_lifecycle(
        owner_b58: &str,
        merkle_root_hex: &str,
        lifecycle: u8,
    ) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{
                "merkle_root": "{merkle_root_hex}",
                "owner": "{owner_b58}",
                "plaintext_size_bytes": 0,
                "stored_size_bytes": 0,
                "chunk_count": 1,
                "fee_pool": 0,
                "created_at": 100,
                "activated_at_height": null,
                "abandoned_at_height": null,
                "assignment_height": 100,
                "visibility": 1,
                "lifecycle": {lifecycle},
                "access_list": []
            }}}}"#
        )
    }

    /// Phase 4c-FIX regression guard: a Private V2 row with
    /// `lifecycle = Pending (0)` MUST refuse with `NotActive` BEFORE
    /// any signing or tx submission. Without this gate the operator
    /// would burn the access-mutation fee against a chain that
    /// rejects Pending-state mutations.
    #[tokio::test]
    async fn require_private_v2_owner_refuses_pending_lifecycle() {
        let owner_addr = [0x33u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);
        let root_hex = format!("0x{}", "42".repeat(32));
        let body = private_v2_info_with_lifecycle(&owner_b58, &root_hex, 0 /* Pending */);
        let url = one_shot_rpc(body).await;
        let rpc = L1RpcClient::new(url);

        let err = require_private_v2_owner(&rpc, &root_hex, &owner_b58)
            .await
            .unwrap_err();
        match err {
            AccessOpError::NotActive { lifecycle } => assert_eq!(lifecycle, 0),
            other => panic!("expected NotActive(Pending), got {other:?}"),
        }
    }

    /// Phase 4c-FIX regression guard: Abandoned (lifecycle = 2) is
    /// terminal — chain rejects any mutation. Refuse pre-tx.
    #[tokio::test]
    async fn require_private_v2_owner_refuses_abandoned_lifecycle() {
        let owner_addr = [0x44u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);
        let root_hex = format!("0x{}", "55".repeat(32));
        let body = private_v2_info_with_lifecycle(&owner_b58, &root_hex, 2 /* Abandoned */);
        let url = one_shot_rpc(body).await;
        let rpc = L1RpcClient::new(url);

        let err = require_private_v2_owner(&rpc, &root_hex, &owner_b58)
            .await
            .unwrap_err();
        match err {
            AccessOpError::NotActive { lifecycle } => assert_eq!(lifecycle, 2),
            other => panic!("expected NotActive(Abandoned), got {other:?}"),
        }
    }

    /// Sanity: Active (lifecycle = 1) passes the lifecycle gate. We
    /// don't run the rest of `share`/`revoke`/`update_access` here —
    /// the K_file round-trip and parser tests already cover those
    /// pieces independently. This test pins ONLY that the lifecycle
    /// gate accepts Active and returns the StorageFileInfoV2 row.
    #[tokio::test]
    async fn require_private_v2_owner_accepts_active_lifecycle() {
        let owner_addr = [0x55u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);
        let root_hex = format!("0x{}", "66".repeat(32));
        let body = private_v2_info_with_lifecycle(&owner_b58, &root_hex, 1 /* Active */);
        let url = one_shot_rpc(body).await;
        let rpc = L1RpcClient::new(url);

        let info = require_private_v2_owner(&rpc, &root_hex, &owner_b58)
            .await
            .expect("Active must pass");
        assert!(info.lifecycle.is_active());
        assert!(info.visibility.is_private());
        assert_eq!(info.owner, owner_b58);
    }

    /// Lifecycle gate fires BEFORE owner check. A non-owner against a
    /// Pending file should see `NotActive` (file is wrong shape) and
    /// not `NotOwner` (which would leak to the requester that the
    /// file exists with a specific owner).
    #[tokio::test]
    async fn require_private_v2_owner_lifecycle_check_precedes_owner_check() {
        let real_owner_addr = [0xAAu8; 20];
        let real_owner_b58 = identity::l1_address_base58(&real_owner_addr);
        let stranger_addr = [0xBBu8; 20];
        let stranger_b58 = identity::l1_address_base58(&stranger_addr);
        let root_hex = format!("0x{}", "77".repeat(32));
        let body = private_v2_info_with_lifecycle(&real_owner_b58, &root_hex, 0 /* Pending */);
        let url = one_shot_rpc(body).await;
        let rpc = L1RpcClient::new(url);

        // Stranger asks; gate refuses on lifecycle, NOT on
        // not-owner. (If the order were reversed, this would surface
        // NotOwner, which would also leak the real owner's address
        // back to a probe attacker.)
        let err = require_private_v2_owner(&rpc, &root_hex, &stranger_b58)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AccessOpError::NotActive { lifecycle: 0 }),
            "expected NotActive ahead of NotOwner; got {err:?}"
        );
    }

    /// `share` produces a bundle the recipient can actually unwrap.
    /// This is the proof that the K_file recovery + recipient wrap
    /// pipeline is end-to-end correct, completely independent of any
    /// chain RPC.
    #[tokio::test]
    async fn share_e2e_recipient_can_unwrap_k_file_from_owner_built_bundle() {
        use sum_crypto::{unwrap_for_self, wrap_for_recipient, x25519_keypair_from_ed25519_seed};
        use sum_types::rpc_types::{LifecycleV2, VisibilityV2};

        // Owner setup.
        let owner_seed = [0xA1u8; 32];
        let (_, owner_pk) = x25519_keypair_from_ed25519_seed(&owner_seed);
        let owner_addr = [0x33u8; 20];
        let owner_b58 = identity::l1_address_base58(&owner_addr);

        // Recipient setup.
        let recipient_seed = [0xB2u8; 32];
        let (recipient_sk, recipient_pk) = x25519_keypair_from_ed25519_seed(&recipient_seed);
        let recipient_addr = [0x44u8; 20];

        // K_file used at ingest, wrapped for owner.
        let k_file_plain = [0xCDu8; 32];
        let owner_bundle = wrap_for_recipient(&k_file_plain, &owner_addr, &owner_pk).unwrap();
        let info = StorageFileInfoV2 {
            merkle_root: "0x".to_string() + &"42".repeat(32),
            owner: owner_b58.clone(),
            plaintext_size_bytes: 0,
            stored_size_bytes: 0,
            chunk_count: 0,
            fee_pool: 0,
            created_at: 0,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: 0,
            visibility: VisibilityV2::PRIVATE,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: vec![AccessEntryV2 {
                address: owner_b58.clone(),
                encrypted_key_bundle: Some(format!("0x{}", hex::encode(owner_bundle))),
                expires_at: None,
            }],
        };
        let rpc = L1RpcClient::new("http://127.0.0.1:0".into());

        // Owner recovers K_file from their own bundle.
        let k_file_recovered = recover_k_file_from_owner_bundle(
            &rpc,
            &owner_seed,
            owner_addr,
            &owner_b58,
            &info,
            "0xroot",
        )
        .await
        .unwrap();
        assert_eq!(*k_file_recovered, k_file_plain);

        // Owner wraps for the new recipient.
        let new_bundle =
            wrap_for_recipient(&k_file_recovered, &recipient_addr, &recipient_pk).unwrap();

        // Recipient unwraps with their own X25519 secret.
        let recipient_sk_z: Zeroizing<[u8; 32]> = Zeroizing::new(recipient_sk);
        let recipient_recovered =
            unwrap_for_self(&new_bundle, &recipient_sk_z, &recipient_addr).unwrap();
        assert_eq!(
            recipient_recovered, k_file_plain,
            "recipient must recover the same K_file the owner ingested with"
        );
    }
}
