//! JSON-RPC client for communicating with the SUM Chain L1 node.
//!
//! Uses `reqwest` for async HTTP and raw JSON-RPC 2.0 framing.
//! Wraps the `storage_*` endpoints and transaction submission.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::debug;

use sum_types::rpc_types::{
    AssignmentCoverageV2, BlockHeightInfo, ChainParamsInfo, ChallengeInfo, NodeRecordInfo,
    StorageFileInfo, StorageFileInfoV2, TxStatusV2,
};

/// JSON-RPC client connected to a SUM Chain L1 node.
///
/// `Clone` is cheap: `reqwest::Client` already shares its connection
/// pool internally, and `rpc_url` is a `String` clone. This lets V2
/// components (PushValidator, AssignmentAttestor, V2Dispatcher) each
/// own their own typed handle while still sharing the connection pool.
#[derive(Clone)]
pub struct L1RpcClient {
    client: reqwest::Client,
    rpc_url: String,
}

impl L1RpcClient {
    /// Create a new RPC client targeting the given URL.
    pub fn new(rpc_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            rpc_url,
        }
    }

    /// Low-level JSON-RPC 2.0 call (public for e2e-helper).
    pub async fn call_public<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        self.call(method, params).await
    }

    /// Low-level JSON-RPC 2.0 call.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        debug!(%method, "RPC call");

        let resp = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .context("RPC HTTP request failed")?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("failed to read RPC response body")?;

        if !status.is_success() {
            anyhow::bail!("RPC HTTP error {status}: {text}");
        }

        let json: Value = serde_json::from_str(&text).context("RPC response is not valid JSON")?;

        if let Some(err) = json.get("error") {
            anyhow::bail!("RPC error: {err}");
        }

        let result = json.get("result").cloned().unwrap_or(Value::Null);

        serde_json::from_value(result).context("failed to deserialize RPC result")
    }

    // ── Storage endpoints ─────────────────────────────────────────────────

    /// Query the access list for a file by its merkle root.
    ///
    /// `merkle_root_hex` must be 0x-prefixed (e.g., `"0xabcd..."`).
    /// Returns `None` if the file is not registered on the L1.
    pub async fn get_access_list(&self, merkle_root_hex: &str) -> Result<Option<StorageFileInfo>> {
        self.call("storage_getAccessList", json!([merkle_root_hex]))
            .await
    }

    /// Get all active PoR challenges targeting this node.
    ///
    /// `node_addr_base58` is the node's L1 address in base58 format.
    pub async fn get_active_challenges(
        &self,
        node_addr_base58: &str,
    ) -> Result<Vec<ChallengeInfo>> {
        self.call("storage_getActiveChallenges", json!([node_addr_base58]))
            .await
    }

    /// Get all files with a non-zero fee pool (eligible for storage rewards).
    pub async fn get_funded_files(&self) -> Result<Vec<StorageFileInfo>> {
        self.call("storage_getFundedFiles", json!([])).await
    }

    /// Get the node registry record for an address.
    pub async fn get_node_record(&self, node_addr_base58: &str) -> Result<Option<NodeRecordInfo>> {
        self.call("storage_getNodeRecord", json!([node_addr_base58]))
            .await
    }

    // ── Transaction endpoints ─────────────────────────────────────────────

    /// Submit a hex-encoded signed transaction to the L1 mempool.
    ///
    /// Returns the transaction hash on success. Tolerates two
    /// chain wire shapes:
    ///
    ///   1. Bare hex string: `"0xabc..."`
    ///   2. Wrapped object:  `{"tx_hash": "0xabc..."}`
    ///
    /// Both are observed in different chain-mirror builds; SNIP
    /// extracts the hex hash from either form. Anything else (a
    /// non-string non-object, an object missing `tx_hash`, an
    /// object with a non-string `tx_hash`) surfaces as a typed
    /// error so callers can't accidentally pass a JSON blob into
    /// `chain_getTransactionStatus`.
    pub async fn send_raw_transaction(&self, hex: &str) -> Result<String> {
        let raw: Value = self.call("send_raw_transaction", json!([hex])).await?;
        extract_tx_hash(&raw)
    }

    /// Get the current nonce for an account.
    pub async fn get_nonce(&self, addr_base58: &str) -> Result<u64> {
        self.call("get_nonce", json!([addr_base58])).await
    }

    /// Get the chain ID.
    pub async fn get_chain_id(&self) -> Result<u64> {
        self.call("chain_id", json!([])).await
    }

    // ── V2 RPC endpoints (chain plan v3.2 §4) ─────────────────────────────
    //
    // These complement the V1 methods above; legacy V1 files keep using
    // `get_access_list`, `get_funded_files`, etc. Active-node lookups are
    // routed through `storage_get_active_nodes_at_height` against the
    // finalized head rather than the unsupported `storage_getActiveNodes`.
    // Phase 0b's ingest / push validator / attestor / finality waiter call
    // the V2 variants exclusively.

    /// `chain_getBlockHeight(["finalized"])` — finalized chain head.
    ///
    /// Always passes the explicit `"finalized"` param; without it the
    /// chain defaults missing/null/`"latest"` to the latest-included
    /// height, which makes safety-critical checks (V2-enabled gate,
    /// abandon grace, terminal-failed reorg windows) racy. Every caller
    /// that compares "are we past block X" wants finalized, so we do
    /// not expose a non-finalized variant — if a caller ever genuinely
    /// needs the latest head, add a separate method with explicit
    /// `["latest"]` rather than overloading this one.
    pub async fn chain_get_block_height(&self) -> Result<BlockHeightInfo> {
        self.call("chain_getBlockHeight", json!(["finalized"]))
            .await
    }

    /// `chain_getChainParams` — live consensus + V2 protocol constants.
    ///
    /// Read at startup; values are protocol-constant per genesis (a
    /// change would be a hard fork). Used by W10 ingest to populate
    /// `IngestParams` from chain rather than baking defaults.
    pub async fn chain_get_chain_params(&self) -> Result<ChainParamsInfo> {
        self.call("chain_getChainParams", json!([])).await
    }

    /// `chain_getTransactionStatus(tx_hash)` — V2 finality primitive.
    /// Wire shape: `{"kind": "...", ...}` with internally-tagged
    /// variants (chain-confirmed; earlier drafts said `"status"`).
    ///
    /// W9 (`tx_wait::wait_for_finalized`) is the canonical caller — see
    /// the `TxStatusV2` doc-comment in `sum_types::rpc_types` for
    /// per-variant semantics.
    pub async fn chain_get_transaction_status(&self, tx_hash: &str) -> Result<TxStatusV2> {
        self.call("chain_getTransactionStatus", json!([tx_hash]))
            .await
    }

    /// `storage_getFileInfoV2` — V2 file row + paginated access list.
    ///
    /// "File not found" surfaces as a JSON `null` result, which decodes
    /// to `Ok(None)`; a present row decodes to `Ok(Some(..))`. The three
    /// return states are therefore explicit and distinguishable:
    ///
    ///   * present row (JSON object) → `Ok(Some(StorageFileInfoV2))`
    ///   * absent row (JSON `null`) → `Ok(None)`
    ///   * malformed row (object of the wrong shape) → decode `Err`
    ///   * JSON-RPC error response → typed RPC `Err`
    ///   * transport / HTTP failure → typed transport `Err`
    ///
    /// Callers must handle `Ok(None)` explicitly (a "not registered"
    /// signal distinct from any error) rather than folding not-found into
    /// the error channel.
    ///
    /// `access_offset`/`access_limit` paginate the access list; pass
    /// `None`/`None` for the chain default (offset=0, limit=256).
    /// Public-file ingests typically have empty access lists, so the
    /// pagination is a no-op for Phase 0b.
    pub async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
        access_offset: Option<u32>,
        access_limit: Option<u32>,
    ) -> Result<Option<StorageFileInfoV2>> {
        self.call(
            "storage_getFileInfoV2",
            json!([merkle_root_hex, access_offset, access_limit]),
        )
        .await
    }

    /// `storage_getActiveNodesAtHeight(h)` — Ask 15 height-based snapshot.
    /// Walks back to the most recent snapshot ≤ `h`; the genesis
    /// snapshot at h=0 is guaranteed to exist (chain plan §5.3).
    ///
    /// Snapshots are immutable per height, so callers should cache
    /// these aggressively — same `h` always yields the same response.
    pub async fn storage_get_active_nodes_at_height(
        &self,
        height: u64,
    ) -> Result<Vec<NodeRecordInfo>> {
        self.call("storage_getActiveNodesAtHeight", json!([height]))
            .await
    }

    /// `account_getEncryptionPublicKey(addr)` — fetch the X25519
    /// encryption public key the address has registered (Phase 4 — Private
    /// files). Returns `None` when the account has no key on chain;
    /// callers must treat that as "this recipient cannot receive private
    /// shares yet" rather than silently dropping them.
    ///
    /// **Chain-confirmed wire shape**: `Option<"0x" + 64-char-lowercase-hex>`.
    /// We additionally tolerate the `0x` prefix being absent (defensive)
    /// and uppercase hex (case-insensitive `hex::decode`) so a future
    /// chain RPC-layer convention change doesn't immediately break
    /// every existing client. Anything else (wrong length, non-hex)
    /// surfaces as `Err` — silently returning `None` would let an
    /// upgrade bug look like "no recipients have registered yet."
    pub async fn account_get_encryption_public_key(
        &self,
        addr_base58: &str,
    ) -> Result<Option<[u8; 32]>> {
        let raw: Option<String> = self
            .call("account_getEncryptionPublicKey", json!([addr_base58]))
            .await?;
        let Some(s) = raw else {
            return Ok(None);
        };
        let stripped = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(stripped)
            .with_context(|| format!("encryption pubkey for {addr_base58} is not valid hex"))?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "encryption pubkey for {addr_base58} must be 32 bytes, got {}",
                bytes.len()
            )
        })?;
        Ok(Some(arr))
    }

    /// `storage_getAssignmentCoverageV2` — bitmap coverage state for
    /// a Pending/Active V2 file.
    ///
    /// "File not found" surfaces as a JSON `null` result → `Ok(None)`; a
    /// present coverage row decodes to `Ok(Some(..))`. The return states
    /// mirror `storage_get_file_info_v2`:
    ///
    ///   * present row (JSON object) → `Ok(Some(AssignmentCoverageV2))`
    ///   * absent row (JSON `null`) → `Ok(None)`
    ///   * malformed row (object of the wrong shape) → decode `Err`
    ///   * JSON-RPC error response → typed RPC `Err`
    ///   * transport / HTTP failure → typed transport `Err`
    ///
    /// Callers must handle `Ok(None)` explicitly (a terminal "not found"
    /// signal) rather than treating not-found as an opaque error.
    ///
    /// `missing_offset` is a **chunk-index lower bound**, not an offset
    /// into the missing list. Callers paginating uncovered chunks should
    /// cycle `missing_offset = last_returned_index + 1` per chain plan
    /// §4 line 474. `missing_limit` defaults to 1024 client-side per
    /// the W9-confirmed semantics; the chain caps it at 16384.
    pub async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
        missing_offset: Option<u32>,
        missing_limit: Option<u32>,
    ) -> Result<Option<AssignmentCoverageV2>> {
        self.call(
            "storage_getAssignmentCoverageV2",
            json!([merkle_root_hex, missing_offset, missing_limit]),
        )
        .await
    }

    /// URL this client is connected to.
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
}

/// Extract the transaction hash from `send_raw_transaction`'s
/// response. The chain has shipped two wire shapes across builds:
///
///   * Bare hex string — `"0xabc..."`
///   * Wrapped object — `{"tx_hash": "0xabc..."}`
///
/// Both decode here; anything else (non-string non-object, object
/// missing `tx_hash`, object with non-string `tx_hash`) surfaces as
/// a typed error. Pulled out as a free function so the contract
/// tests can pin both shapes without spinning up an HTTP server
/// per case.
fn extract_tx_hash(raw: &Value) -> Result<String> {
    if let Some(s) = raw.as_str() {
        return Ok(s.to_string());
    }
    if let Some(obj) = raw.as_object() {
        if let Some(field) = obj.get("tx_hash") {
            if let Some(s) = field.as_str() {
                return Ok(s.to_string());
            }
            anyhow::bail!("send_raw_transaction returned object with non-string tx_hash: {raw}");
        }
        anyhow::bail!("send_raw_transaction returned object without tx_hash field: {raw}");
    }
    anyhow::bail!("send_raw_transaction returned non-string non-object value: {raw}")
}

// ── Contract tests ───────────────────────────────────────────────────────────
//
// These verify the JSON-RPC wire shape we assume for V2 endpoints. Each
// test stands up a one-shot in-process HTTP responder on an ephemeral
// port, serves a single canned response, and asserts the client's
// behavior.
//
// **Why a contract test for `storage_getFileInfoV2` matters**: the
// method returns `Result<Option<StorageFileInfoV2>>`, so the wire shape
// maps to three distinct client states that callers rely on being kept
// apart: a present row (object → `Ok(Some)`), an absent row (`null` →
// `Ok(None)`), and a malformed row / JSON-RPC error / transport failure
// (all → `Err`). The tests below pin each of these paths so the shape
// contract — especially "`null` is not-found, not an error" — can't
// drift unnoticed.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot HTTP responder. Returns the URL the client
    /// should target. The responder accepts exactly one connection,
    /// drains the request body, and writes back the canned response.
    async fn one_shot_responder(response_body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the inbound request — we don't inspect it here, just
            // need to clear the socket buffer before writing the response.
            // HTTP/1.1 with Content-Length means we can read a fixed
            // amount; for brevity, read until we've seen \r\n\r\n + the
            // declared body length (or just read what's available, which
            // is enough for our single-shot use case).
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body,
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        url
    }

    /// Valid object → `Ok(Some(..))`. Client must decode without error
    /// and surface the row wrapped in `Some`.
    #[tokio::test]
    async fn storage_get_file_info_v2_object_is_some() {
        let body = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "merkle_root": "0xabcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234",
                "owner": "OwnerB58Addr",
                "plaintext_size_bytes": 10485760,
                "stored_size_bytes": 10485760,
                "chunk_count": 10,
                "fee_pool": 100000,
                "created_at": 12345,
                "activated_at_height": null,
                "assignment_height": 12345,
                "visibility": 0,
                "lifecycle": 0,
                "access_list": []
            }
        }"#
        .to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let info = client
            .storage_get_file_info_v2(
                "0xabcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234",
                None,
                None,
            )
            .await
            .expect("client must decode success shape")
            .expect("present row must be Some");
        assert_eq!(info.chunk_count, 10);
        assert!(info.lifecycle.is_pending());
        assert!(info.visibility.is_public());
    }

    /// "File not found" path: chain returns a JSON-RPC error object.
    /// Client must surface this as `Err`, NOT silently as `Ok` of
    /// some sentinel value. This is the reviewer-required test that
    /// pins the not-found wire shape explicitly.
    #[tokio::test]
    async fn storage_get_file_info_v2_jsonrpc_error_surfaces_as_err() {
        let body = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32602,
                "message": "file not registered: 0x000...000"
            }
        }"#
        .to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_file_info_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        let err = result.expect_err("expected RPC error, got Ok");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RPC error") || msg.contains("file not registered"),
            "error message must surface the JSON-RPC error: {msg}"
        );
    }

    /// `null` result → `Ok(None)`: the chain reports the file has no V2
    /// row. This MUST decode as `Ok(None)` (a distinct not-found signal),
    /// never as an `Err` and never as a fabricated default row.
    #[tokio::test]
    async fn storage_get_file_info_v2_null_is_none() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_file_info_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await
            .expect("null result must decode as Ok(None), not Err");
        assert!(
            result.is_none(),
            "null result must be Ok(None); got {result:?}"
        );
    }

    /// Malformed object (valid JSON, wrong shape for `StorageFileInfoV2`)
    /// → decode `Err`. A wrong-shaped object must NOT be silently coerced
    /// to `None`; it is a real error the caller fails closed on.
    #[tokio::test]
    async fn storage_get_file_info_v2_malformed_object_is_err() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"unexpected":"shape"}}"#.to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_file_info_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        assert!(
            result.is_err(),
            "malformed object must be a decode Err, not Ok(None); got {result:?}"
        );
    }

    /// Transport failure (socket accepted then closed with no HTTP
    /// response) → transport `Err`. Never `Ok(None)`.
    #[tokio::test]
    async fn storage_get_file_info_v2_transport_failure_is_err() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server =
            start_mock_rpc(routes([("storage_getFileInfoV2", MockResponse::Hangup)])).await;
        let client = L1RpcClient::new(server.url());
        let result = client
            .storage_get_file_info_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        assert!(
            result.is_err(),
            "transport failure must be an Err, not Ok(None); got {result:?}"
        );
    }

    // ── storage_getAssignmentCoverageV2 contract tests (#38) ─────────────

    /// A complete coverage row body. `per_archive` is required; the epoch
    /// fields default when absent.
    fn coverage_v2_body() -> String {
        r#"{
            "chunk_count": 10,
            "covered_count": 10,
            "can_activate_now": true,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": []
        }"#
        .to_string()
    }

    /// Valid object → `Ok(Some(..))`.
    #[tokio::test]
    async fn storage_get_assignment_coverage_v2_object_is_some() {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{}}}"#,
            coverage_v2_body()
        );
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let cov = client
            .storage_get_assignment_coverage_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await
            .expect("client must decode coverage shape")
            .expect("present coverage row must be Some");
        assert_eq!(cov.chunk_count, 10);
        assert!(cov.can_activate_now);
    }

    /// `null` result → `Ok(None)` (not-found signal, not an error).
    #[tokio::test]
    async fn storage_get_assignment_coverage_v2_null_is_none() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_assignment_coverage_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await
            .expect("null result must decode as Ok(None)");
        assert!(
            result.is_none(),
            "null coverage must be Ok(None); got {result:?}"
        );
    }

    /// Malformed object (wrong shape) → decode `Err`.
    #[tokio::test]
    async fn storage_get_assignment_coverage_v2_malformed_object_is_err() {
        let body =
            r#"{"jsonrpc":"2.0","id":1,"result":{"chunk_count":"not-a-number"}}"#.to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_assignment_coverage_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        assert!(
            result.is_err(),
            "malformed coverage object must be a decode Err; got {result:?}"
        );
    }

    /// JSON-RPC error response → typed RPC `Err`.
    #[tokio::test]
    async fn storage_get_assignment_coverage_v2_jsonrpc_error_is_err() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal error"}}"#
            .to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let result = client
            .storage_get_assignment_coverage_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        assert!(result.is_err(), "JSON-RPC error must be an Err");
    }

    /// Transport failure → transport `Err` (never `Ok(None)`).
    #[tokio::test]
    async fn storage_get_assignment_coverage_v2_transport_failure_is_err() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([(
            "storage_getAssignmentCoverageV2",
            MockResponse::Hangup,
        )]))
        .await;
        let client = L1RpcClient::new(server.url());
        let result = client
            .storage_get_assignment_coverage_v2(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                None,
                None,
            )
            .await;
        assert!(
            result.is_err(),
            "transport failure must be an Err, not Ok(None); got {result:?}"
        );
    }

    /// Chain-confirmed wire shape for
    /// `account_getEncryptionPublicKey`: `Option<0x-prefixed lowercase hex32>`.
    /// Pin the canonical chain shape end-to-end (from RPC bytes to
    /// `[u8; 32]`) so a future chain-side prefixing or casing change
    /// is caught at this layer.
    #[tokio::test]
    async fn account_get_encryption_public_key_chain_canonical_shape() {
        let body = r#"{"jsonrpc":"2.0","id":1,
            "result": "0x1111111111111111111111111111111111111111111111111111111111111111"
        }"#
        .to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let pk = client
            .account_get_encryption_public_key("OwnerB58Addr")
            .await
            .expect("decode chain-canonical lowercase 0x-prefixed hex32");
        assert_eq!(pk, Some([0x11; 32]));
    }

    /// `null` → `None`: the account has no key registered. SNIP must
    /// surface this as `Ok(None)` so the caller can decide whether to
    /// abort the ingest (locked decision: refuse a Private file
    /// targeting a recipient with no chain key) vs. continue.
    #[tokio::test]
    async fn account_get_encryption_public_key_null_is_none() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string();
        let url = one_shot_responder(body).await;
        let client = L1RpcClient::new(url);
        let pk = client
            .account_get_encryption_public_key("OwnerB58Addr")
            .await
            .expect("null result must decode as Ok(None)");
        assert_eq!(pk, None);
    }

    /// Defensive forward-compat: tolerate uppercase hex and absent
    /// `0x` prefix (some early-test chain builds emitted these). This
    /// is intentional flex on the SNIP side; the chain commits only to
    /// the lowercase 0x-prefixed shape, so this test pins SNIP's
    /// laxer-than-spec acceptance — a future regression that becomes
    /// strict-only would fire here.
    #[tokio::test]
    async fn account_get_encryption_public_key_tolerates_legacy_shapes() {
        for body_str in [
            // Bare hex, no 0x.
            r#"{"jsonrpc":"2.0","id":1,
                "result":"1111111111111111111111111111111111111111111111111111111111111111"}"#,
            // Uppercase hex, with prefix.
            r#"{"jsonrpc":"2.0","id":1,
                "result":"0x1111111111111111111111111111111111111111111111111111111111111111"}"#,
            // Mixed case.
            r#"{"jsonrpc":"2.0","id":1,
                "result":"0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa"}"#,
        ] {
            let url = one_shot_responder(body_str.to_string()).await;
            let client = L1RpcClient::new(url);
            let pk = client
                .account_get_encryption_public_key("OwnerB58Addr")
                .await
                .expect("legacy shape should decode");
            assert!(pk.is_some());
        }
    }

    /// Pathological inputs: wrong length / non-hex MUST surface as
    /// `Err`, NOT as `Ok(None)`. Silently returning `None` would let
    /// an upgrade bug look like "no recipients registered."
    #[tokio::test]
    async fn account_get_encryption_public_key_invalid_payload_errors() {
        for body_str in [
            // Odd-length hex.
            r#"{"jsonrpc":"2.0","id":1,"result":"0x111"}"#,
            // Too short (16 bytes instead of 32).
            r#"{"jsonrpc":"2.0","id":1,
                "result":"0x11111111111111111111111111111111"}"#,
            // Non-hex characters.
            r#"{"jsonrpc":"2.0","id":1,
                "result":"0xZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ"}"#,
        ] {
            let url = one_shot_responder(body_str.to_string()).await;
            let client = L1RpcClient::new(url);
            let res = client
                .account_get_encryption_public_key("OwnerB58Addr")
                .await;
            assert!(
                res.is_err(),
                "malformed payload must NOT silently decode as None; got {res:?}"
            );
        }
    }

    // ── send_raw_transaction wire-shape tests ────────────────────────

    /// Bare hex string — the canonical V2 chain shape.
    #[test]
    fn extract_tx_hash_accepts_bare_string() {
        let v = serde_json::json!("0xabcdef0123");
        assert_eq!(extract_tx_hash(&v).unwrap(), "0xabcdef0123");
    }

    /// Wrapped object — what a current chain mirror build emits.
    /// SNIP MUST tolerate this shape so the tx-hash isn't passed
    /// downstream as a JSON blob.
    #[test]
    fn extract_tx_hash_accepts_wrapped_object() {
        let v = serde_json::json!({"tx_hash": "0xdeadbeef"});
        assert_eq!(extract_tx_hash(&v).unwrap(), "0xdeadbeef");
    }

    /// Object with extra fields alongside `tx_hash` is still
    /// extracted — extra metadata (e.g., a future `block_hint`)
    /// shouldn't break us as long as `tx_hash` is present.
    #[test]
    fn extract_tx_hash_accepts_object_with_extra_fields() {
        let v = serde_json::json!({
            "tx_hash": "0xfoobar",
            "ack_height": 1234,
            "future_field": "ignored",
        });
        assert_eq!(extract_tx_hash(&v).unwrap(), "0xfoobar");
    }

    /// Object missing `tx_hash` field is a typed error — never
    /// silently passes a JSON blob into downstream code.
    #[test]
    fn extract_tx_hash_rejects_object_without_tx_hash_field() {
        let v = serde_json::json!({"hash": "0xabc"});
        let err = extract_tx_hash(&v).expect_err("missing tx_hash must error");
        assert!(
            err.to_string().contains("without tx_hash field"),
            "error must explain why: {err}"
        );
    }

    /// Object with a non-string `tx_hash` (e.g., chain bug emits
    /// number) is a typed error.
    #[test]
    fn extract_tx_hash_rejects_object_with_non_string_tx_hash() {
        let v = serde_json::json!({"tx_hash": 12345});
        let err = extract_tx_hash(&v).expect_err("non-string tx_hash must error");
        assert!(
            err.to_string().contains("non-string tx_hash"),
            "error must explain why: {err}"
        );
    }

    /// Non-string non-object response (number, bool, null, array)
    /// is a typed error.
    #[test]
    fn extract_tx_hash_rejects_other_shapes() {
        for bogus in [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!(["0xabc"]),
        ] {
            let err = extract_tx_hash(&bogus).expect_err("must error on {bogus}");
            assert!(
                err.to_string().contains("non-string non-object"),
                "error for {bogus} should mention shape: {err}"
            );
        }
    }

    // ── Active-node routing wire-shape tests (#37) ───────────────────────

    /// (a) `chain_get_block_height` MUST call `chain_getBlockHeight` with
    /// exactly `["finalized"]` — never latest/head — and decode the
    /// height. The finalized param is safety-critical: latest/head would
    /// make active-node snapshots racy.
    #[tokio::test]
    async fn chain_get_block_height_uses_finalized_param() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([(
            "chain_getBlockHeight",
            MockResponse::Result(json!({"height": 4242, "finality": "finalized"})),
        )]))
        .await;
        let client = L1RpcClient::new(server.url());
        let info = client
            .chain_get_block_height()
            .await
            .expect("finalized height must decode");
        assert_eq!(info.height, 4242);
        assert_eq!(info.finality, "finalized");
        assert_eq!(
            server.first_params("chain_getBlockHeight"),
            Some(json!(["finalized"])),
            "must request the finalized head explicitly"
        );
    }

    /// (b) `storage_get_active_nodes_at_height(h)` MUST call
    /// `storage_getActiveNodesAtHeight` with exactly `[h]` — the precise
    /// height passed by the caller.
    #[tokio::test]
    async fn storage_get_active_nodes_at_height_passes_exact_height() {
        use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
        let server = start_mock_rpc(routes([(
            "storage_getActiveNodesAtHeight",
            MockResponse::Result(json!([])),
        )]))
        .await;
        let client = L1RpcClient::new(server.url());
        let nodes = client
            .storage_get_active_nodes_at_height(9001)
            .await
            .expect("at-height snapshot must decode");
        assert!(nodes.is_empty());
        assert_eq!(
            server.first_params("storage_getActiveNodesAtHeight"),
            Some(json!([9001])),
            "must query the exact height requested"
        );
        assert_eq!(
            server.method_count("storage_getActiveNodesAtHeight"),
            1,
            "exactly one at-height query"
        );
    }

    /// (g) Negative source scan: no code anywhere in this crate issues a
    /// JSON-RPC call to the unsupported bare active-nodes method (the one
    /// WITHOUT the `AtHeight` suffix). The bare method's fully-quoted
    /// literal is distinct from the supported height-pinned variant, so
    /// this catches any regression that reintroduces the orphan endpoint.
    /// The needle is assembled at runtime so this scan never matches its
    /// own source.
    #[test]
    fn no_source_calls_bare_storage_get_active_nodes() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Assemble the fully-quoted bare method literal from fragments so
        // neither this line nor the surrounding docs contain it verbatim.
        let bare = ["storage_get", "ActiveNodes"].concat();
        let needle = format!("\"{bare}\"");
        let needle = needle.as_str();
        let mut offenders = Vec::new();
        visit_rs_files(&src_dir, &mut |path, contents| {
            if contents.contains(needle) {
                offenders.push(path.display().to_string());
            }
        });
        assert!(
            offenders.is_empty(),
            "bare storage_getActiveNodes call literal found in: {offenders:?}"
        );
    }

    /// Recursively read every `.rs` file under `dir`, invoking `f` with
    /// the path and file contents.
    fn visit_rs_files(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit_rs_files(&path, f);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    f(&path, &contents);
                }
            }
        }
    }
}
