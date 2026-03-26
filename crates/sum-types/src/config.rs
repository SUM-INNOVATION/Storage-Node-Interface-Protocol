// Global configuration structs for the SUM Storage Node.

use std::path::PathBuf;

// ── Networking ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NetConfig {
    /// UDP port for QUIC listener. 0 = OS-assigned.
    pub listen_port: u16,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self { listen_port: 0 }
    }
}

// ── Storage ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Root directory for chunk files and manifests.
    /// Defaults to `$HOME/.sumnode/store/`.
    pub store_dir: PathBuf,

    /// Maximum bytes per P2P chunk transfer message.
    /// Chunks larger than this are fetched in multiple round-trips.
    /// Default: 2 MiB.
    pub max_chunk_msg_bytes: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        let store_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/sumnode"))
            .join(".sumnode")
            .join("store");

        Self {
            store_dir,
            max_chunk_msg_bytes: 2 * 1024 * 1024,
        }
    }
}

// ── L1 RPC ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// URL of the SUM Chain L1 JSON-RPC endpoint.
    pub rpc_url: String,

    /// How often (seconds) the PoR worker polls for active challenges.
    pub por_poll_interval_secs: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            rpc_url: "http://127.0.0.1:9944".to_string(),
            por_poll_interval_secs: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_config_defaults() {
        let cfg = StoreConfig::default();
        assert_eq!(cfg.max_chunk_msg_bytes, 2 * 1024 * 1024);
        assert!(cfg.store_dir.ends_with("store"));
    }
}
