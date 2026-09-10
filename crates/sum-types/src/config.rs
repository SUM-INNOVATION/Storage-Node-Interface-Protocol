// Global configuration structs for the SUM Storage Node.

use std::path::PathBuf;

// ── Networking ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct NetConfig {
    /// UDP port for the QUIC listener. `0` = OS-assigned (ephemeral).
    ///
    /// Setting an explicit port is required for stable WAN connectivity
    /// when the node is behind a NAT and depends on UDP hole-punching
    /// or UPnP-forwarded UDP. With an OS-assigned port the public UDP
    /// port changes on every restart, breaking DCUtR and any
    /// pre-configured port forwards.
    pub udp_listen_port: u16,
    /// TCP port for the Noise+Yamux listener. `0` = OS-assigned.
    ///
    /// Same caveat as [`Self::udp_listen_port`] — pin a stable port for
    /// any peer that needs to be reliably dialable from the WAN.
    pub tcp_listen_port: u16,
    /// Enable WAN discovery via Kademlia DHT + TCP transport.
    /// When false, only mDNS (LAN) is used.
    pub enable_wan: bool,
    /// Bootstrap peer multiaddrs for Kademlia DHT.
    /// Example: `/ip4/1.2.3.4/tcp/4001/p2p/12D3KooW...`
    pub bootstrap_peers: Vec<String>,
    /// Volunteer this node as a Circuit Relay v2 server.
    /// Only enable on publicly-reachable hosts (VPS, port-forwarded home
    /// server). Does nothing unless `enable_wan` is also true.
    pub relay_server: bool,
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

/// Environment variable naming the store root. Lower precedence than
/// the `--store-dir` flag, higher than `$HOME`.
pub const STORE_DIR_ENV: &str = "SUM_STORE_DIR";

/// Default cap on bytes per P2P chunk transfer message (2 MiB).
pub const DEFAULT_MAX_CHUNK_MSG_BYTES: usize = 2 * 1024 * 1024;

/// Resolve the store root, highest precedence first:
///
/// 1. `flag` — the `--store-dir` CLI flag.
/// 2. `$SUM_STORE_DIR`.
/// 3. `$HOME/.sumnode/store`.
///
/// Every path is used verbatim; no CWD-relative resolution happens
/// here, and none should. `SumStore` has never consulted the process
/// working directory, so a relative path is resolved by the operating
/// system against whatever CWD the process happens to hold — which is
/// exactly the ambiguity the flag exists to remove.
pub fn resolve_store_dir(flag: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = flag {
        return dir;
    }
    if let Some(dir) = std::env::var_os(STORE_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/sumnode"))
        .join(".sumnode")
        .join("store")
}

impl StoreConfig {
    /// Build a store config with the root resolved by
    /// [`resolve_store_dir`] and the default message cap.
    pub fn resolve(store_dir_flag: Option<PathBuf>) -> Self {
        Self {
            store_dir: resolve_store_dir(store_dir_flag),
            max_chunk_msg_bytes: DEFAULT_MAX_CHUNK_MSG_BYTES,
        }
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self::resolve(None)
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

    /// Every test that reads or writes `$HOME` or `SUM_STORE_DIR` takes this.
    ///
    /// The environment is process-wide, so a test that mutates it races every
    /// other test that reads it. Without the lock these pass alone and fail
    /// under `cargo test`'s default threading — the worst way for a test to be
    /// wrong, because it looks like flakiness rather than a missing invariant.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn store_config_defaults() {
        // Takes the same lock as the resolution tests below: this reads $HOME
        // through `default()`, and they mutate it. Without the lock this test
        // passes alone and flakes under `cargo test`'s default threading.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = StoreConfig::default();
        assert_eq!(cfg.max_chunk_msg_bytes, 2 * 1024 * 1024);
        assert!(cfg.store_dir.ends_with("store"));
    }

    // ── Store-root resolution ────────────────────────────────────────────
    //
    // The precedence order is the whole point, so it is tested as an order
    // rather than as three independent lookups.

    struct EnvGuard {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let prior = std::env::var_os(key);
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
            EnvGuard { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn the_flag_wins_over_the_env_and_over_home() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, Some("/from/env"));
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(
            resolve_store_dir(Some(PathBuf::from("/from/flag"))),
            PathBuf::from("/from/flag")
        );
    }

    #[test]
    fn the_env_wins_over_home_when_no_flag_is_given() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, Some("/from/env"));
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(resolve_store_dir(None), PathBuf::from("/from/env"));
    }

    #[test]
    fn home_is_the_fallback_and_the_path_is_the_documented_one() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(
            resolve_store_dir(None),
            PathBuf::from("/from/home/.sumnode/store"),
            "the runbook documents this exact path"
        );
    }

    /// An empty `SUM_STORE_DIR` is not a store root. Treating it as one would
    /// resolve every node to the filesystem root's `.sumnode/store`.
    #[test]
    fn an_empty_env_value_is_ignored_rather_than_used() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, Some(""));
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(
            resolve_store_dir(None),
            PathBuf::from("/from/home/.sumnode/store")
        );
    }

    /// The working directory has never been consulted, and must not start
    /// being. This is the claim the operator runbook got wrong.
    #[test]
    fn the_working_directory_is_never_consulted() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        let from_here = resolve_store_dir(None);
        let cwd = std::env::current_dir().expect("a working directory");
        assert!(
            !from_here.starts_with(&cwd) || cwd == std::path::Path::new("/from/home"),
            "resolution must not be relative to {}",
            cwd.display()
        );
    }
}
