//! ACL (Access Control List) enforcement for chunk and manifest serving.
//!
//! Before serving a chunk or manifest to a peer, queries the L1's
//! `storage_getAccessList` to verify the requester is authorized. Maps
//! `PeerId` → L1 address using the libp2p identify protocol's exchanged
//! public keys.
//!
//! # Profile-gated policy
//!
//! Three branches return "uncertain" outcomes that depend on the runtime
//! profile (see [`crate::profile::NodeProfile`]):
//!
//! 1. **Unknown CID** — request CID is neither `manifest:<hex>` nor a
//!    chunk indexed locally. Production: deny. Dev: allow.
//! 2. **File not registered on L1** — `storage_getAccessList` returned
//!    `None`. Production: deny. Dev: allow.
//! 3. **L1 RPC error** — could not reach the chain. Production: deny.
//!    Dev: allow. This is the [`AclChecker::check_access_or_default`]
//!    path used by the listen and ingest serve loops.
//!
//! All three were "allow" pre-Phase-0a — the v1 demo profile that needed
//! to work without an L1 chain at all. Production must fail closed; Dev
//! retains the old behaviour with loud warnings, gated by an explicit
//! `--profile dev` flag.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use sum_net::PeerId;
use tokio::sync::RwLock;
use tracing::warn;

use sum_net::identity;
use sum_store::ManifestIndex;
use sum_store::serve::MANIFEST_REQUEST_PREFIX;

use crate::profile::NodeProfile;
use crate::rpc_client::L1RpcClient;

/// Checks whether a peer is allowed to retrieve a chunk or manifest.
pub struct AclChecker {
    rpc: Arc<L1RpcClient>,
    /// PeerId -> L1 Address mapping, populated by `PeerIdentified` events.
    peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    profile: NodeProfile,
}

impl AclChecker {
    pub fn new(
        rpc: Arc<L1RpcClient>,
        peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
        profile: NodeProfile,
    ) -> Self {
        Self { rpc, peer_addresses, profile }
    }

    /// Whether this checker is running in Production mode. Useful for
    /// callers that want to log the policy decision they're about to make.
    pub fn profile(&self) -> NodeProfile {
        self.profile
    }

    /// Check if `peer_id` is allowed to retrieve `cid`.
    ///
    /// Returns:
    /// - `Ok(true)` — access allowed
    /// - `Ok(false)` — access denied
    /// - `Err(_)` — RPC failure; the caller should resolve via the active
    ///   profile (use [`Self::check_access_or_default`] for the typical
    ///   serve-loop path that already encodes that policy).
    pub async fn check_access(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> Result<bool> {
        // 1. Resolve the request CID to a file's merkle root.
        //    - `manifest:<hex>`: extract the root from the prefix. The
        //      manifest itself is gated by the file's ACL.
        //    - chunk CID: look up via the local manifest index.
        let root = match resolve_root(cid, manifest_index) {
            Some(r) => r,
            None => {
                // CID isn't recognised. Could be a stray request, a chunk
                // we don't index locally, or a malformed manifest prefix.
                return Ok(self.uncertain_branch_allows("unknown CID", peer_id, cid));
            }
        };

        // 2. Query L1 for the file's access list.
        let root_hex = format!(
            "0x{}",
            root.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let file_info = self.rpc.get_access_list(&root_hex).await?;

        let Some(info) = file_info else {
            // File not registered on L1.
            return Ok(self.uncertain_branch_allows("file not registered on L1", peer_id, cid));
        };

        // 3. Empty access list ⇒ public file, anyone may read.
        if info.access_list.is_empty() {
            return Ok(true);
        }

        // 4. Look up the requester's L1 address.
        let peer_addr = {
            let map = self.peer_addresses.read().await;
            map.get(peer_id).copied()
        };
        let Some(addr) = peer_addr else {
            warn!(
                %peer_id,
                "ACL check: peer's L1 address unknown (identify not yet received) — denying"
            );
            return Ok(false);
        };

        // 5. Check if the peer's address is in the access list.
        let addr_base58 = identity::l1_address_base58(&addr);
        Ok(info.access_list.contains(&addr_base58))
    }

    /// Same as [`Self::check_access`] but RPC errors are resolved against
    /// the active profile: deny in Production, allow in Dev (with a
    /// `warn!` line either way).
    pub async fn check_access_or_default(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> bool {
        match self.check_access(peer_id, cid, manifest_index).await {
            Ok(allowed) => allowed,
            Err(e) => match self.profile {
                NodeProfile::Production => {
                    warn!(
                        %peer_id, %cid, %e,
                        "ACL check failed (RPC error) — DENYING (profile=production)"
                    );
                    false
                }
                NodeProfile::Dev => {
                    warn!(
                        %peer_id, %cid, %e,
                        "ACL check failed (RPC error) — allowing (profile=dev)"
                    );
                    true
                }
            },
        }
    }

    /// Centralised policy for the two "uncertain" branches (unknown CID,
    /// unregistered file). Production denies; Dev allows with a warning.
    fn uncertain_branch_allows(&self, reason: &'static str, peer_id: &PeerId, cid: &str) -> bool {
        match self.profile {
            NodeProfile::Production => {
                warn!(
                    %peer_id, %cid, %reason,
                    "ACL: DENYING (profile=production)"
                );
                false
            }
            NodeProfile::Dev => {
                warn!(
                    %peer_id, %cid, %reason,
                    "ACL: allowing (profile=dev)"
                );
                true
            }
        }
    }
}

/// Resolve a request CID to the merkle root of the file it references.
/// Returns `None` for CIDs that don't correspond to any known file.
fn resolve_root(cid: &str, manifest_index: &ManifestIndex) -> Option<[u8; 32]> {
    if let Some(hex) = cid.strip_prefix(MANIFEST_REQUEST_PREFIX) {
        return parse_hex_to_32(hex);
    }
    manifest_index.merkle_root_for_cid(cid).copied()
}

/// Parse a 64-char lowercase-hex string into a 32-byte array. Returns
/// `None` for any other shape — including uppercase, which `from_str_radix`
/// would otherwise accept. The protocol emits CIDs via `format!("{b:02x}")`
/// (always lowercase) so we keep the canonical encoding here.
fn parse_hex_to_32(hex: &str) -> Option<[u8; 32]> {
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

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn empty_index() -> (TempDir, ManifestIndex) {
        let dir = TempDir::new().expect("tempdir");
        let idx = ManifestIndex::load(dir.path()).expect("load empty manifest index");
        (dir, idx)
    }

    #[test]
    fn manifest_cid_resolves_to_root() {
        let (_dir, idx) = empty_index();
        let cid = "manifest:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let root = resolve_root(cid, &idx).expect("manifest CID must resolve");
        assert_eq!(root[0], 0x01);
        assert_eq!(root[31], 0xef);
    }

    #[test]
    fn manifest_cid_with_bad_hex_returns_none() {
        let (_dir, idx) = empty_index();
        let too_short = "manifest:abcd";
        assert!(resolve_root(too_short, &idx).is_none());

        let non_hex =
            "manifest:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdez";
        assert!(resolve_root(non_hex, &idx).is_none());
    }

    #[test]
    fn unknown_chunk_cid_returns_none() {
        let (_dir, idx) = empty_index();
        assert!(resolve_root("bafk_unknown", &idx).is_none());
    }

    #[test]
    fn parse_hex_to_32_rejects_uppercase_and_wrong_length() {
        // 64 chars but uppercase letters in positions 56-63: must reject
        // (previous test only happened to exercise the length branch).
        assert!(
            parse_hex_to_32("0123456789abcdef0123456789abcdef0123456789abcdef01234567ABCDEF89")
                .is_none(),
            "uppercase hex must be rejected"
        );
        // Mixed-case is also out — only canonical lowercase is accepted.
        assert!(
            parse_hex_to_32("Abcd5678abcdef0123456789abcdef0123456789abcdef0123456789abcdef01")
                .is_none(),
            "mixed-case hex must be rejected"
        );
        // 63 chars: short.
        assert!(
            parse_hex_to_32("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde")
                .is_none()
        );
        // 65 chars: long.
        assert!(
            parse_hex_to_32("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0")
                .is_none()
        );
        // Sanity: canonical lowercase 64 chars is accepted.
        assert!(
            parse_hex_to_32("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_some()
        );
    }
}
