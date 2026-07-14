//! ACL (Access Control List) enforcement for chunk and manifest serving.
//!
//! Before serving a chunk or manifest to a peer, the checker resolves
//! the requested CID to a file root, queries the chain for the file's
//! visibility and access list, and verifies the requester is
//! authorized. `PeerId` → L1 address mapping comes from the libp2p
//! identify protocol's exchanged public keys.
//!
//! # V2-aware dispatch (Phase 4b)
//!
//! [`AclChecker::check_access`] tries `storage_getFileInfoV2` first
//! and gates on `visibility`:
//!
//!   * **Public V2** — open read for any peer.
//!   * **Private V2** — requester L1 address must be in
//!     `access_list`, and if the matched entry sets `expires_at` the
//!     finalized chain head must not yet exceed it (strict-greater rule).
//!
//! `storage_getFileInfoV2` now returns `Result<Option<StorageFileInfoV2>>`.
//! The three outcomes route as follows — **fail closed on every error**:
//!
//!   * **`Ok(Some(row))`** → apply the V2 ACL above.
//!   * **`Ok(None)`** (JSON `null` — the V2-aware chain's authoritative
//!     "no V2 row" representation) → the ONLY signal that permits a V1
//!     `storage_getAccessList` fallback for a genuinely legacy file.
//!   * **`Err` of ANY kind** — JSON-RPC `-32601` / "not registered" /
//!     "file not found" / "unknown root", `-32603` internal error,
//!     malformed JSON, result-decode error, HTTP non-200, transport
//!     error, or timeout → **deny; V1 is NOT consulted**. An error is
//!     never authoritative absence: reading one as "legacy-absent"
//!     would let anyone who can induce a V2 error (a mangled response,
//!     an induced timeout) downgrade a Private file from V2 ACL
//!     enforcement to the laxer V1 path.
//!
//! # Profile-gated policy
//!
//! Three branches return "uncertain" outcomes that depend on the runtime
//! profile (see [`crate::profile::NodeProfile`]):
//!
//! 1. **Unknown CID** — request CID is neither `manifest:<hex>` nor a
//!    chunk indexed locally (Public CBOR index OR Private cid-to-root
//!    sidecar). Production: deny. Dev: allow.
//! 2. **File not registered on L1** — V2 returned `Ok(None)` AND V1
//!    `storage_getAccessList` returned `None`. Production: deny. Dev: allow.
//! 3. **L1 RPC error** — `check_access` returned `Err`: the V1 path
//!    failed, OR the V2 call returned ANY error (every V2 error fails
//!    closed here). Production: deny. Dev: allow. This is
//!    the [`AclChecker::check_access_or_default`] path used by the
//!    listen and ingest serve loops.
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
        Self {
            rpc,
            peer_addresses,
            profile,
        }
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
    ///
    /// Phase 4b dispatch order:
    ///   1. Resolve CID → merkle root via `manifest_index` (Public CBOR
    ///      index OR Private cid->root sidecar).
    ///   2. Try `storage_getFileInfoV2`. If the row exists, gate on
    ///      visibility:
    ///        * Public V2  → allow when access list is empty (public
    ///          read), otherwise membership check (rare in practice).
    ///        * Private V2 → require requester L1 address in
    ///          `access_list` AND `expires_at` not yet exceeded by
    ///          finalized height.
    ///   3. If V2 returns `Ok(None)` (JSON `null` — the chain is
    ///      V2-aware but has no V2 row for this root) — and ONLY then —
    ///      fall back to V1 `storage_getAccessList`. This keeps
    ///      genuinely legacy V1 files working without forcing them
    ///      through the V2 RPC surface.
    ///   4. **Privacy-first fail-closed**: ANY V2 error — JSON-RPC
    ///      `-32601` / "not registered" / "file not found" / "unknown
    ///      root", `-32603` internal, HTTP non-200, transport error,
    ///      malformed JSON, result-decode error, or timeout — does NOT
    ///      fall back to V1. The error propagates so
    ///      `check_access_or_default` denies in Production. An error is
    ///      never authoritative absence; falling back would let anyone
    ///      who can induce a V2 error downgrade to the laxer V1 ACL
    ///      path on a Private file.
    pub async fn check_access(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> Result<bool> {
        // 1. Resolve the request CID to a file's merkle root.
        //    - `manifest:<hex>`: extract the root from the prefix. The
        //      manifest itself is gated by the file's ACL.
        //    - chunk CID: look up via the local manifest index (now
        //      Phase-4b-aware: consults both Public and Private maps).
        let root = match resolve_root(cid, manifest_index) {
            Some(r) => r,
            None => {
                // CID isn't recognised. Could be a stray request, a chunk
                // we don't index locally, or a malformed manifest prefix.
                return Ok(self.uncertain_branch_allows("unknown CID", peer_id, cid));
            }
        };
        let root_hex = format!(
            "0x{}",
            root.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );

        // 2. V2-aware path. `storage_getFileInfoV2` returns the V2 row
        //    with visibility, AccessEntryV2 (with bundles + expires_at).
        //    ONLY an explicit `Ok(None)` (JSON `null`) is authoritative
        //    absence and may consult V1; every `Err` fails closed.
        match self
            .rpc
            .storage_get_file_info_v2(&root_hex, None, None)
            .await
        {
            Ok(Some(info)) => {
                return self.check_access_v2(peer_id, cid, &info).await;
            }
            Ok(None) => {
                // Chain is V2-aware and reports no V2 row for this root
                // (JSON `null` → `Ok(None)`). This is the ONLY authoritative
                // not-found signal — safe to consult V1 for a legacy file.
                tracing::debug!(
                    %peer_id, %cid,
                    "ACL: V2 file_info returned no row (null) — falling back to V1 storage_getAccessList"
                );
            }
            Err(e) => {
                // FAIL CLOSED on EVERY V2 error — JSON-RPC -32601 /
                // "not registered" / "file not found" / "unknown root",
                // -32603 internal, malformed JSON, decode error, HTTP
                // non-200, transport error, or timeout. An error is never
                // authoritative absence: consulting V1 here would let
                // anyone able to induce a V2 error downgrade a Private
                // file from V2 ACL enforcement to the laxer V1 path.
                // Production resolves this Err to deny via
                // `check_access_or_default`; V1 is NOT called.
                tracing::warn!(
                    %peer_id, %cid, %e,
                    "ACL: V2 file_info errored — denying, no V1 fallback (only Ok(None) may consult V1)"
                );
                return Err(e);
            }
        }

        // 3. V1 legacy fallback (existing behavior, byte-identical).
        let file_info = self.rpc.get_access_list(&root_hex).await?;

        let Some(info) = file_info else {
            // File not registered on L1 (V2 or V1).
            return Ok(self.uncertain_branch_allows("file not registered on L1", peer_id, cid));
        };

        // Empty access list ⇒ public V1 file, anyone may read.
        if info.access_list.is_empty() {
            return Ok(true);
        }

        let Some(addr) = self.resolve_peer_addr(peer_id).await else {
            return Ok(false);
        };
        let addr_base58 = identity::l1_address_base58(&addr);
        Ok(info.access_list.contains(&addr_base58))
    }

    /// V2 access-list gate (chain plan v3.2 §3.1). Public files are
    /// open-read; Private files require the requester to be in the
    /// access list AND, if the entry has `expires_at`, the chain's
    /// finalized height must not yet exceed it.
    async fn check_access_v2(
        &self,
        peer_id: &PeerId,
        cid: &str,
        info: &sum_types::rpc_types::StorageFileInfoV2,
    ) -> Result<bool> {
        // Public V2: by design open to all readers. The chain may carry
        // an empty `access_list` for Public files, or omit access
        // semantics entirely. We don't enforce a per-peer membership
        // check here — that's the Private branch's job.
        if info.visibility.is_public() {
            return Ok(true);
        }

        // Private V2: require explicit access entry.
        let Some(addr) = self.resolve_peer_addr(peer_id).await else {
            return Ok(false);
        };
        let addr_base58 = identity::l1_address_base58(&addr);
        let Some(entry) = info.access_list.iter().find(|e| e.address == addr_base58) else {
            tracing::info!(
                %peer_id, %cid,
                addr = %addr_base58,
                root = %info.merkle_root,
                "ACL (V2 Private): peer not in access_list — denying"
            );
            return Ok(false);
        };

        // Expiry: only when the entry sets one. `chain_get_block_height`
        // returns finalized height post-PR-#13 (explicit `["finalized"]`
        // param), so this comparison is reorg-safe. Strict-greater
        // matches chain semantics.
        if let Some(expires_at) = entry.expires_at {
            let head = self.rpc.chain_get_block_height().await?;
            if head.height > expires_at {
                tracing::info!(
                    %peer_id, %cid,
                    addr = %addr_base58,
                    finalized = head.height,
                    expires_at,
                    "ACL (V2 Private): access expired — denying"
                );
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Common helper: resolve a `PeerId` to its L1 address via the
    /// libp2p identify map. Logs a warning and returns `None` when
    /// identify hasn't fired yet (the V1 ACL also denied that case).
    async fn resolve_peer_addr(&self, peer_id: &PeerId) -> Option<[u8; 20]> {
        let map = self.peer_addresses.read().await;
        let result = map.get(peer_id).copied();
        if result.is_none() {
            warn!(
                %peer_id,
                "ACL check: peer's L1 address unknown (identify not yet received) — denying"
            );
        }
        result
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn empty_index() -> (TempDir, ManifestIndex) {
        let dir = TempDir::new().expect("tempdir");
        let idx = ManifestIndex::load(dir.path()).expect("load empty manifest index");
        (dir, idx)
    }

    /// Spawn an HTTP responder that serves a fixed FIFO queue of JSON
    /// bodies, one per inbound connection. Returns the URL the client
    /// should target. Reqwest opens a fresh TCP connection per JSON-RPC
    /// call (no pooling under our sequential ACL flow), so each `bodies`
    /// entry maps 1:1 to a chain RPC issued by `check_access`.
    async fn queued_responder(bodies: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        tokio::spawn(async move {
            for body in bodies {
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
            }
        });
        url
    }

    fn rpc_result(json_body: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{json_body}}}"#)
    }
    fn rpc_error(code: i32, msg: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":{code},"message":"{msg}"}}}}"#)
    }

    /// Build an `AclChecker` whose RPC points at the test responder
    /// AND whose peer-address map is pre-populated with the supplied
    /// `(peer, l1_addr)` mappings. `profile = Production` so the
    /// "uncertain" branches deny — tests that expect deny don't have
    /// to switch profile.
    async fn build_checker(url: &str, peers: Vec<(PeerId, [u8; 20])>) -> AclChecker {
        let rpc = Arc::new(L1RpcClient::new(url.to_string()));
        let map = HashMap::from_iter(peers);
        let peer_addresses = Arc::new(RwLock::new(map));
        AclChecker::new(rpc, peer_addresses, NodeProfile::Production)
    }

    fn fake_peer() -> PeerId {
        sum_net::Keypair::generate_ed25519().public().to_peer_id()
    }

    /// `StorageFileInfoV2` response body. Constructs the JSON shape
    /// the chain emits — see `crates/sum-types/src/rpc_types.rs`.
    fn v2_info_json(merkle_root_hex: &str, visibility: u8, access_list_json: &str) -> String {
        format!(
            r#"{{
                "merkle_root": "{merkle_root_hex}",
                "owner": "OwnerB58",
                "plaintext_size_bytes": 0,
                "stored_size_bytes": 0,
                "chunk_count": 1,
                "fee_pool": 0,
                "created_at": 100,
                "activated_at_height": 150,
                "abandoned_at_height": null,
                "assignment_height": 100,
                "visibility": {visibility},
                "lifecycle": 1,
                "access_list": {access_list_json}
            }}"#
        )
    }
    fn v1_info_json(merkle_root_hex: &str, access_list: &[&str], fee_pool: u64) -> String {
        let list: Vec<String> = access_list.iter().map(|a| format!(r#""{a}""#)).collect();
        format!(
            r#"{{
                "merkle_root": "{merkle_root_hex}",
                "owner": "OwnerB58",
                "total_size_bytes": 0,
                "access_list": [{list}],
                "fee_pool": {fee_pool},
                "created_at": 0
            }}"#,
            list = list.join(",")
        )
    }
    fn block_height_json(height: u64) -> String {
        format!(r#"{{"height": {height}, "finality": "finalized"}}"#)
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

        let non_hex = "manifest:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdez";
        assert!(resolve_root(non_hex, &idx).is_none());
    }

    #[test]
    fn unknown_chunk_cid_returns_none() {
        let (_dir, idx) = empty_index();
        assert!(resolve_root("bafk_unknown", &idx).is_none());
    }

    /// Phase 4b: a Private (encrypted) chunk's CID lives in
    /// `private_cid_to_root` (not `cid_to_root`, since the manifest
    /// is encrypted and has no decoded chunk list). `resolve_root`
    /// MUST find it via `merkle_root_for_cid`'s combined lookup,
    /// otherwise the ACL gate returns "unknown CID" and authorized
    /// recipients get denied on every Private chunk pull.
    #[test]
    fn private_chunk_cid_resolves_via_combined_index() {
        let dir = TempDir::new().expect("tempdir");
        let mut idx = ManifestIndex::load(dir.path()).expect("load empty manifest index");
        let private_root = [0xC4u8; 32];
        let private_cid = "bafk_private_resolve";
        idx.record_private_chunk_cid(private_root, private_cid)
            .expect("record private cid");

        let resolved = resolve_root(private_cid, &idx)
            .expect("Private CID must resolve, otherwise ACL denies authorized recipients");
        assert_eq!(resolved, private_root);
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

    // ── Phase 4b: V2-aware ACL behavior ────────────────────────────────

    /// Authorized Private V2 peer is allowed: chain's V2 row carries
    /// the peer's L1 address in `access_list`, no `expires_at`, so
    /// only one RPC (V2 file_info) is issued.
    #[tokio::test]
    async fn v2_private_authorized_peer_allowed() {
        let (_dir, mut idx) = empty_index();
        let root_hex = "0xabababababababababababababababababababababababababababababababab";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        // Stage a private chunk CID so resolve_root finds the file.
        let cid = "bafk_authorized_chunk";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let peer = fake_peer();
        let peer_addr = [0xCDu8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        // V2 PRIVATE = 1; access_list contains the peer.
        let access_list = format!(
            r#"[{{"address": "{peer_b58}", "encrypted_key_bundle": "0x{}", "expires_at": null}}]"#,
            "AB".repeat(80)
        );
        let v2_body = v2_info_json(root_hex, 1, &access_list);
        let url = queued_responder(vec![rpc_result(&v2_body)]).await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let allowed = acl
            .check_access(&peer, cid, &idx)
            .await
            .expect("RPC succeeded");
        assert!(allowed, "authorized Private peer must be allowed");
    }

    /// Unauthorized Private V2 peer is denied: peer's L1 address is
    /// NOT in the access list. Single V2 RPC, then `check_access`
    /// returns `Ok(false)` without consulting V1.
    #[tokio::test]
    async fn v2_private_unauthorized_peer_denied() {
        let (_dir, mut idx) = empty_index();
        let root_hex = "0xbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbc";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        let cid = "bafk_unauthorized_chunk";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let attacker = fake_peer();
        let attacker_addr = [0xEEu8; 20];
        // The chain's access_list lists a *different* address — owner only.
        let owner_b58 = identity::l1_address_base58(&[0x11u8; 20]);
        let access_list = format!(
            r#"[{{"address": "{owner_b58}", "encrypted_key_bundle": "0x{}", "expires_at": null}}]"#,
            "AB".repeat(80)
        );
        let v2_body = v2_info_json(root_hex, 1, &access_list);
        let url = queued_responder(vec![rpc_result(&v2_body)]).await;
        let acl = build_checker(&url, vec![(attacker, attacker_addr)]).await;

        let allowed = acl
            .check_access(&attacker, cid, &idx)
            .await
            .expect("RPC succeeded");
        assert!(!allowed, "unauthorized Private peer must be denied");
    }

    /// Expired access denial: peer is in the access list, but
    /// `expires_at` is set and the chain's *finalized* head is past
    /// it. Two RPCs: V2 file_info → block_height. Strict-greater rule.
    #[tokio::test]
    async fn v2_private_expired_access_denied() {
        let (_dir, mut idx) = empty_index();
        let root_hex = "0xdededededededededededededededededededededededededededededededede";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        let cid = "bafk_expired_chunk";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let peer = fake_peer();
        let peer_addr = [0xCDu8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        // Access entry has expires_at = 1000.
        let access_list = format!(
            r#"[{{"address": "{peer_b58}", "encrypted_key_bundle": "0x{}", "expires_at": 1000}}]"#,
            "AB".repeat(80)
        );
        let v2_body = v2_info_json(root_hex, 1, &access_list);
        // Finalized head = 2000 → strictly greater than expires_at → deny.
        let url = queued_responder(vec![
            rpc_result(&v2_body),
            rpc_result(&block_height_json(2000)),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let allowed = acl
            .check_access(&peer, cid, &idx)
            .await
            .expect("RPC succeeded");
        assert!(!allowed, "expired Private access must be denied");
    }

    /// Boundary: finalized height == expires_at is still ALLOWED
    /// (strict > means equal-to is fine). Pin the canonical chain rule.
    #[tokio::test]
    async fn v2_private_access_at_exact_expiry_still_allowed() {
        let (_dir, mut idx) = empty_index();
        let root_hex = "0xefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        let cid = "bafk_boundary_chunk";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let peer = fake_peer();
        let peer_addr = [0x55u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        let access_list = format!(
            r#"[{{"address": "{peer_b58}", "encrypted_key_bundle": "0x{}", "expires_at": 1000}}]"#,
            "AB".repeat(80)
        );
        let v2_body = v2_info_json(root_hex, 1, &access_list);
        let url = queued_responder(vec![
            rpc_result(&v2_body),
            rpc_result(&block_height_json(1000)),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let allowed = acl
            .check_access(&peer, cid, &idx)
            .await
            .expect("RPC succeeded");
        assert!(
            allowed,
            "head == expires_at must still be allowed (strict >)"
        );
    }

    /// V2 lookup fails with `-32601` (Method not found). This is an
    /// ERROR, not the authoritative `Ok(None)` — it MUST fail closed
    /// with NO V1 fallback. The V1 body queued below would allow the
    /// peer; asserting `Err` proves V1 was never consulted.
    #[tokio::test]
    async fn v2_method_not_found_denies_no_v1_fallback() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "abababababababababababababababababababababababababababababababab";
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x77u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        let v1_body = v1_info_json(&root_hex_prefixed, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            rpc_error(-32601, "Method not found"),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 -32601 error MUST deny (Err) with no V1 fallback; got {result:?}"
        );
    }

    /// V2 lookup fails with a chain-side "file not registered" message.
    /// It is an ERROR, not `Ok(None)`, so it MUST deny with no V1
    /// fallback even though the queued V1 body would allow the peer.
    #[tokio::test]
    async fn v2_file_not_registered_denies_no_v1_fallback() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x88u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        let v1_body = v1_info_json(&root_hex_prefixed, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            rpc_error(-32602, "file not registered"),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 'file not registered' error MUST deny (Err) with no V1 fallback; got {result:?}"
        );
    }

    /// **Privacy-critical regression guard**: an ambiguous V2 chain
    /// error (here -32603 "internal error") MUST NOT trigger V1
    /// fallback. If it did, an attacker who could induce the chain
    /// to return -32603 for V2 lookups (or a man-in-the-middle who
    /// can mangle V2 responses) would force the laxer V1 ACL path on
    /// a Private file. The check must instead surface the error so
    /// `check_access_or_default` denies in Production.
    #[tokio::test]
    async fn v2_internal_error_does_not_fall_back_to_v1() {
        let (_dir, mut idx) = empty_index();
        // Stage a *Private* CID so resolve_root finds it; the test
        // verifies the V2 path doesn't downgrade to V1 even though
        // V1 access_list (queued but should never be consumed) would
        // ALLOW this peer.
        let root_hex = "0xabcd00abcd00abcd00abcd00abcd00abcd00abcd00abcd00abcd00abcd00abcd";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        let cid = "bafk_private_fail_closed";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let peer = fake_peer();
        let peer_addr = [0x77u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        // V2 returns -32603 internal error → ambiguous, must NOT
        // fall back. The V1 body queued below would (incorrectly)
        // grant access if we fell back; the test asserts we don't.
        let v1_body = v1_info_json(root_hex, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            rpc_error(-32603, "internal chain error"),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, cid, &idx).await;
        assert!(
            result.is_err(),
            "ambiguous V2 error MUST surface as Err so production denies; got {result:?}"
        );
    }

    /// **Privacy-critical regression guard**: a V2 transport-layer
    /// failure (HTTP non-200, malformed body, decode failure) MUST
    /// NOT trigger V1 fallback. We simulate it with a 200-OK
    /// response carrying a body that isn't valid JSON. The
    /// `L1RpcClient` reports this as "RPC response is not valid
    /// JSON" — an `Err`, so it fails closed (only `Ok(None)` may
    /// consult V1).
    #[tokio::test]
    async fn v2_decode_error_does_not_fall_back_to_v1() {
        let (_dir, mut idx) = empty_index();
        let root_hex = "0xefef00efef00efef00efef00efef00efef00efef00efef00efef00efef00efef";
        let root = parse_hex_to_32(root_hex.strip_prefix("0x").unwrap()).unwrap();
        let cid = "bafk_private_decode_fail";
        idx.record_private_chunk_cid(root, cid).unwrap();

        let peer = fake_peer();
        let peer_addr = [0x77u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        // V2 responder sends garbage instead of JSON. Reqwest reads
        // the body, serde_json::from_str fails, rpc_client returns
        // "RPC response is not valid JSON". V1 body is queued (and
        // would allow the peer), but classifier must keep us from
        // calling V1 at all.
        let v1_body = v1_info_json(root_hex, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            "this is not valid json".to_string(),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 decode failure MUST surface as Err so production denies; got {result:?}"
        );
    }

    /// V2 error whose message says "file not found" → still an ERROR,
    /// not `Ok(None)` → deny, no V1 fallback.
    #[tokio::test]
    async fn v2_file_not_found_error_denies_no_v1_fallback() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "1f".repeat(32);
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");
        let peer = fake_peer();
        let peer_addr = [0x88u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);
        let v1_body = v1_info_json(&root_hex_prefixed, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            rpc_error(-32000, "file not found"),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;
        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 'file not found' error MUST deny; got {result:?}"
        );
    }

    /// V2 error whose message says "unknown root" → deny, no V1 fallback.
    #[tokio::test]
    async fn v2_unknown_root_error_denies_no_v1_fallback() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "2e".repeat(32);
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");
        let peer = fake_peer();
        let peer_addr = [0x88u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);
        let v1_body = v1_info_json(&root_hex_prefixed, &[&peer_b58], 1000);
        let url = queued_responder(vec![
            rpc_error(-32000, "unknown root"),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;
        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 'unknown root' error MUST deny; got {result:?}"
        );
    }

    /// V2 `null` (Ok(None)) → V1 fallback, but V1 lists a DIFFERENT
    /// address → requester not listed → deny.
    #[tokio::test]
    async fn v2_null_then_v1_unlisted_peer_denies() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "3d".repeat(32);
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");
        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];
        let other_b58 = identity::l1_address_base58(&[0x11u8; 20]);
        let v1_body = v1_info_json(&root_hex_prefixed, &[&other_b58], 1000);
        let url = queued_responder(vec![rpc_result("null"), rpc_result(&v1_body)]).await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;
        let allowed = acl
            .check_access(&peer, &cid, &idx)
            .await
            .expect("both RPCs returned cleanly");
        assert!(
            !allowed,
            "V2 null → V1 fallback → unlisted peer must be denied"
        );
    }

    /// V2 `null` (Ok(None)) → V1 fallback, but V1 returns a malformed
    /// object → decode Err → deny.
    #[tokio::test]
    async fn v2_null_then_v1_malformed_denies() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "4c".repeat(32);
        let cid = format!("manifest:{root_hex_raw}");
        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];
        let url = queued_responder(vec![
            rpc_result("null"),
            rpc_result(r#"{"unexpected":"v1shape"}"#),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;
        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 null → V1 malformed must deny (Err); got {result:?}"
        );
    }

    /// Public V2 file: visibility = 0, `check_access` returns true
    /// without examining the access list (Public files are open-read).
    #[tokio::test]
    async fn v2_public_open_read_for_any_peer() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefe";
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");

        let v2_body = v2_info_json(&root_hex_prefixed, 0, "[]");
        let url = queued_responder(vec![rpc_result(&v2_body)]).await;
        // No peer address registered — irrelevant for Public.
        let acl = build_checker(&url, vec![]).await;

        let stranger = fake_peer();
        let allowed = acl
            .check_access(&stranger, &cid, &idx)
            .await
            .expect("RPC succeeded");
        assert!(allowed, "Public V2 must be open-read for any peer");
    }

    // ── ACL decision table under Result<Option<StorageFileInfoV2>> (#38) ──
    //
    // Rows (fail-closed in Production):
    //   * V2 Some                            → apply V2 ACL
    //   * V2 None + one V1 record            → V1 clean-legacy fallback
    //   * V2 None + no V1 record             → deny (not registered)
    //   * malformed V2 object                → deny (decode Err)
    //   * V2 RPC error (ambiguous)           → deny (Err)
    //   * V2 transport error                 → deny (Err)
    //   * V2 None + V1 lookup errors         → deny (Err)  [ambiguous legacy]
    //
    // "V2 Some" rows are exercised by the visibility tests above; the
    // ambiguous-RPC-error row by `v2_internal_error_does_not_fall_back_to_v1`.
    // Note: V1 `storage_getAccessList` returns a single `Option`, so
    // "exactly one" vs "no record" are the only representable cardinalities;
    // a genuinely ambiguous V1 outcome can only arrive as a lookup error,
    // which the last row denies.

    /// V2 `null` (Ok(None)) → clean not-found signal → V1 fallback. The
    /// V1 access list lists the peer → allow.
    #[tokio::test]
    async fn v2_null_result_with_v1_record_falls_back_and_allows() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "12".repeat(32);
        let root_hex_prefixed = format!("0x{root_hex_raw}");
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];
        let peer_b58 = identity::l1_address_base58(&peer_addr);

        let v1_body = v1_info_json(&root_hex_prefixed, &[&peer_b58], 1000);
        // First RPC: storage_getFileInfoV2 → null (Ok(None)). Second:
        // storage_getAccessList → V1 record listing the peer.
        let url = queued_responder(vec![rpc_result("null"), rpc_result(&v1_body)]).await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let allowed = acl
            .check_access(&peer, &cid, &idx)
            .await
            .expect("V1 fallback after V2 null must succeed");
        assert!(
            allowed,
            "V2 null → V1 fallback → listed peer must be allowed"
        );
    }

    /// V2 `null` + V1 `null` → file not registered anywhere → deny
    /// (Production fails closed).
    #[tokio::test]
    async fn v2_null_result_no_v1_record_denies() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "34".repeat(32);
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];

        let url = queued_responder(vec![rpc_result("null"), rpc_result("null")]).await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let allowed = acl
            .check_access(&peer, &cid, &idx)
            .await
            .expect("both RPCs returned cleanly");
        assert!(!allowed, "V2 null + V1 null → deny in Production");
    }

    /// Malformed V2 object (valid JSON, wrong shape) → decode Err → NO
    /// fallback → surfaces as Err so Production denies.
    #[tokio::test]
    async fn v2_malformed_object_does_not_fall_back() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "56".repeat(32);
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];

        // Only the V2 call should be issued; the malformed object must
        // fail closed rather than fall back to a queued V1 record.
        let v1_body = v1_info_json(&format!("0x{root_hex_raw}"), &["ShouldNeverBeConsulted"], 1);
        let url = queued_responder(vec![
            rpc_result(r#"{"unexpected":"shape"}"#),
            rpc_result(&v1_body),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "malformed V2 object must surface Err (deny), got {result:?}"
        );
    }

    /// V2 transport failure (socket accepted then closed with no HTTP
    /// response) → Err → NO fallback → deny.
    #[tokio::test]
    async fn v2_transport_failure_does_not_fall_back() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let (_dir, idx) = empty_index();
        let root_hex_raw = "78".repeat(32);
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];

        let server =
            start_mock_rpc(routes([("storage_getFileInfoV2", MockResponse::Hangup)])).await;
        let acl = build_checker(&server.url(), vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "V2 transport failure must surface Err (deny), got {result:?}"
        );
    }

    /// V2 `null` → V1 fallback, but the V1 `storage_getAccessList` lookup
    /// itself errors (the only way a genuinely ambiguous legacy outcome
    /// can arise) → surfaces as Err → deny.
    #[tokio::test]
    async fn v2_null_then_ambiguous_v1_lookup_denies() {
        let (_dir, idx) = empty_index();
        let root_hex_raw = "9a".repeat(32);
        let cid = format!("manifest:{root_hex_raw}");

        let peer = fake_peer();
        let peer_addr = [0x99u8; 20];

        let url = queued_responder(vec![
            rpc_result("null"),
            rpc_error(-32000, "chain temporarily unavailable"),
        ])
        .await;
        let acl = build_checker(&url, vec![(peer, peer_addr)]).await;

        let result = acl.check_access(&peer, &cid, &idx).await;
        assert!(
            result.is_err(),
            "ambiguous V1 fallback lookup must surface Err (deny), got {result:?}"
        );
    }
}
