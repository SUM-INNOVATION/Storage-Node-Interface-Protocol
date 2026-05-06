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
//! V2 lookup failures are split into two classes by
//! `v2_error_is_clean_legacy_signal`:
//!
//!   * **Clean legacy signal** (JSON-RPC `-32601` "Method not found",
//!     or chain message saying "not registered" / "file not found" /
//!     "unknown root") → fall back to V1 `storage_getAccessList`.
//!     Keeps legacy V1 files working without forcing them through
//!     the V2 RPC surface.
//!   * **Ambiguous failure** (HTTP non-200, transport error,
//!     malformed JSON, decode error, `-32603` internal error, etc.)
//!     → propagate as `Err`. No V1 fallback. Privacy-first: silently
//!     downgrading to V1 on ambiguous V2 errors would let a
//!     man-in-the-middle who can mangle V2 responses force the laxer
//!     V1 ACL path on a Private file.
//!
//! # Profile-gated policy
//!
//! Three branches return "uncertain" outcomes that depend on the runtime
//! profile (see [`crate::profile::NodeProfile`]):
//!
//! 1. **Unknown CID** — request CID is neither `manifest:<hex>` nor a
//!    chunk indexed locally (Public CBOR index OR Private cid-to-root
//!    sidecar). Production: deny. Dev: allow.
//! 2. **File not registered on L1** — V2 reported a clean
//!    legacy-signal AND V1 `storage_getAccessList` returned `None`.
//!    Production: deny. Dev: allow.
//! 3. **L1 RPC error** — `check_access` returned `Err` (V1 path
//!    failed, OR V2 failed with an ambiguous shape that did NOT
//!    qualify for fallback). Production: deny. Dev: allow. This is
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
    ///   3. If V2 lookup fails with a **clean legacy signal** —
    ///      `code = -32601` (Method not found) or a chain message
    ///      indicating "not registered" / "file not found" / "unknown
    ///      root" — fall back to V1 `storage_getAccessList`. This
    ///      keeps genuinely legacy V1 files working without forcing
    ///      them through the V2 RPC surface.
    ///   4. **Privacy-first fail-closed**: any other V2 failure
    ///      shape — HTTP non-200, transport error, malformed JSON,
    ///      result-decode error, or an unrecognised JSON-RPC error
    ///      code (e.g. -32603 internal error) — does NOT fall back
    ///      to V1. The error propagates so
    ///      `check_access_or_default` denies in Production. Falling
    ///      back on ambiguous failures would let a man-in-the-middle
    ///      who can mangle V2 responses force the laxer V1 ACL path
    ///      on a Private file. See `v2_error_is_clean_legacy_signal`
    ///      below for the exact classifier.
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
        //    We try this first; the result classifier decides whether
        //    a failure is a legitimate "no V2 row / V2 RPC unsupported"
        //    legacy-signal (→ fall back to V1) or an ambiguous infra
        //    failure (→ fail closed via the caller's policy).
        match self
            .rpc
            .storage_get_file_info_v2(&root_hex, None, None)
            .await
        {
            Ok(info) => {
                return self.check_access_v2(peer_id, cid, &info).await;
            }
            Err(e) if v2_error_is_clean_legacy_signal(&e) => {
                // Chain told us "no V2 row" or "method not supported":
                // file is genuinely V1-only or the chain doesn't
                // implement V2 RPCs. Safe to consult V1.
                tracing::debug!(
                    %peer_id, %cid, %e,
                    "ACL: V2 file_info reports legacy/unsupported — falling back to V1 storage_getAccessList"
                );
            }
            Err(e) => {
                // Ambiguous V2 failure: transport flake, HTTP non-200,
                // malformed JSON, decode error, internal chain error,
                // or any other JSON-RPC error code we don't recognise
                // as a clean legacy signal. Privacy-first policy:
                // surface the error to the caller. Production resolves
                // this to deny via `check_access_or_default`; falling
                // back to V1 here would risk a downgrade attack where
                // a malformed-V2-response forces the laxer V1 ACL
                // path on a Private file.
                tracing::warn!(
                    %peer_id, %cid, %e,
                    "ACL: V2 file_info failed ambiguously — refusing V1 fallback (privacy-first)"
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

/// Classify a `storage_getFileInfoV2` error as either a clean
/// "legacy file / V2 unsupported" signal (→ safe to fall back to V1)
/// or an ambiguous failure that MUST NOT trigger fallback.
///
/// Privacy-first rationale: silently falling back on transport flakes
/// or malformed responses would let a man-in-the-middle (or a buggy
/// proxy) downgrade the V2 ACL path to the laxer V1 `storage_getAccessList`,
/// potentially granting access to a Private file the V2 chain row
/// would deny. Production policy is to fail closed on any failure
/// shape we can't read as an unambiguous "this file/method doesn't
/// exist."
///
/// We classify by the top-level Display string. `L1RpcClient::call`
/// emits distinct prefixes for each failure shape:
///
///   * `"RPC error: <json>"` — the chain returned a well-formed
///     JSON-RPC error response. THIS is the only path eligible for
///     fallback, AND only when the embedded code/message indicates
///     the file or method genuinely doesn't exist.
///   * `"RPC HTTP error <status>: ..."` — non-200 HTTP. Ambiguous.
///   * `"RPC HTTP request failed"` — transport (DNS, connect, TLS).
///   * `"failed to read RPC response body"` — partial body.
///   * `"RPC response is not valid JSON"` — malformed body.
///   * `"failed to deserialize RPC result"` — wrong shape / chain
///     emitted a struct we don't model.
///
/// Only the first prefix can possibly indicate "fall back is safe."
/// The remaining shapes ALL fail closed.
fn v2_error_is_clean_legacy_signal(err: &anyhow::Error) -> bool {
    let msg = format!("{err}");
    if !msg.starts_with("RPC error:") {
        return false;
    }
    // The chain emitted a JSON-RPC error response. Restrict fallback
    // to the unambiguous cases:
    //   * `code = -32601` (Method not found): chain is pre-V2 or
    //     simply doesn't expose V2 RPCs on this endpoint.
    //   * Message body says "not registered" / "not found" /
    //     "unknown root": file genuinely doesn't have a V2 row.
    // Any other JSON-RPC error (e.g. -32603 internal error, custom
    // chain error codes we don't recognise) is treated as ambiguous
    // and we fail closed — the chain may have V2 awareness but be
    // returning errors for reasons that don't justify a downgrade.
    let lower = msg.to_lowercase();
    lower.contains("\"code\":-32601")
        || lower.contains("\"code\": -32601")
        || lower.contains("method not found")
        || lower.contains("not registered")
        || lower.contains("file not found")
        || lower.contains("unknown root")
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

    /// V2 lookup fails with the canonical "method not found" error
    /// code (-32601). This is the unambiguous "chain doesn't expose
    /// V2 RPCs" signal; falling back to V1 is safe. The V1 access
    /// list contains the peer → allow.
    #[tokio::test]
    async fn v2_method_not_found_falls_back_to_v1() {
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

        let allowed = acl
            .check_access(&peer, &cid, &idx)
            .await
            .expect("V1 fallback must succeed for method-not-found");
        assert!(allowed, "V1 legacy-fallback path must allow listed peer");
    }

    /// V2 lookup fails with a chain-side "file not registered" message
    /// (any error code but with a recognised legacy-signal phrase).
    /// Same outcome as method-not-found: V1 fallback is safe.
    #[tokio::test]
    async fn v2_file_not_registered_falls_back_to_v1() {
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

        let allowed = acl
            .check_access(&peer, &cid, &idx)
            .await
            .expect("V1 fallback must succeed for file-not-registered");
        assert!(allowed, "V1 legacy-fallback path must allow listed peer");
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
    /// JSON" — distinct from the JSON-RPC error prefix the
    /// classifier accepts.
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

    /// Unit-level coverage of the classifier itself, so future edits
    /// don't widen the fallback policy by accident.
    #[test]
    fn v2_error_classifier_pins_safe_set() {
        // Clean legacy signals → fallback OK.
        let method_nf =
            anyhow::anyhow!(r#"RPC error: {{"code":-32601,"message":"Method not found"}}"#);
        assert!(v2_error_is_clean_legacy_signal(&method_nf));

        let file_nr =
            anyhow::anyhow!(r#"RPC error: {{"code":-32602,"message":"file not registered"}}"#);
        assert!(v2_error_is_clean_legacy_signal(&file_nr));

        // Ambiguous JSON-RPC error → NO fallback.
        let internal =
            anyhow::anyhow!(r#"RPC error: {{"code":-32603,"message":"internal error"}}"#);
        assert!(!v2_error_is_clean_legacy_signal(&internal));

        // Transport / decode failures → NO fallback.
        for non_legacy in [
            "RPC HTTP error 500: server crashed",
            "RPC HTTP request failed",
            "failed to read RPC response body",
            "RPC response is not valid JSON",
            "failed to deserialize RPC result",
        ] {
            let err = anyhow::anyhow!("{non_legacy}");
            assert!(
                !v2_error_is_clean_legacy_signal(&err),
                "must NOT classify {non_legacy:?} as legacy-signal"
            );
        }
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
}
