//! JSON-RPC client for communicating with the SUM Chain L1 node.
//!
//! Uses `reqwest` for async HTTP and raw JSON-RPC 2.0 framing.
//! Wraps the `storage_*` endpoints and transaction submission.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tracing::debug;

use sum_types::rpc_types::{ChallengeInfo, NodeRecordInfo, StorageFileInfo};

/// JSON-RPC client connected to a SUM Chain L1 node.
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
        let text = resp.text().await.context("failed to read RPC response body")?;

        if !status.is_success() {
            anyhow::bail!("RPC HTTP error {status}: {text}");
        }

        let json: Value = serde_json::from_str(&text).context("RPC response is not valid JSON")?;

        if let Some(err) = json.get("error") {
            anyhow::bail!("RPC error: {err}");
        }

        let result = json
            .get("result")
            .cloned()
            .unwrap_or(Value::Null);

        serde_json::from_value(result).context("failed to deserialize RPC result")
    }

    // ── Storage endpoints ─────────────────────────────────────────────────

    /// Query the access list for a file by its merkle root.
    ///
    /// `merkle_root_hex` must be 0x-prefixed (e.g., `"0xabcd..."`).
    /// Returns `None` if the file is not registered on the L1.
    pub async fn get_access_list(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfo>> {
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
        self.call("storage_getFundedFiles", json!([]))
            .await
    }

    /// Get the node registry record for an address.
    pub async fn get_node_record(
        &self,
        node_addr_base58: &str,
    ) -> Result<Option<NodeRecordInfo>> {
        self.call("storage_getNodeRecord", json!([node_addr_base58]))
            .await
    }

    // ── Transaction endpoints ─────────────────────────────────────────────

    /// Submit a hex-encoded signed transaction to the L1 mempool.
    ///
    /// Returns the transaction hash on success.
    pub async fn send_raw_transaction(&self, hex: &str) -> Result<Value> {
        self.call("send_raw_transaction", json!([hex])).await
    }

    /// Get the current nonce for an account.
    pub async fn get_nonce(&self, addr_base58: &str) -> Result<u64> {
        self.call("get_nonce", json!([addr_base58])).await
    }

    /// Get the chain ID.
    pub async fn get_chain_id(&self) -> Result<u64> {
        self.call("chain_id", json!([])).await
    }

    /// URL this client is connected to.
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
}
