// Global configuration structs for the SUM Storage Node.

use std::path::PathBuf;

use crate::error::SumError;

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
    /// Resolved by [`resolve_store_dir`]; `$HOME/.sumnode/store/` is the
    /// last source tried, not a guaranteed default.
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

/// Message shown when no source names a store root. Names the flag, because
/// the flag is the thing the operator can act on.
const NO_ROOT_MESSAGE: &str = "no store root: pass --store-dir <DIR>, or set \
     SUM_STORE_DIR, or set HOME (the default root is $HOME/.sumnode/store)";

/// Resolve the store root, highest precedence first:
///
/// 1. `flag` — the `--store-dir` CLI flag.
/// 2. `$SUM_STORE_DIR`.
/// 3. `$HOME/.sumnode/store`.
///
/// Fails closed when none of the three names a root. There is deliberately no
/// fourth fallback: the previous one was `/tmp/sumnode`, a fixed path shared by
/// every user on the host, silently adopted exactly when the environment was
/// least well understood. A node that cannot say where its store is must not
/// start.
///
/// Every path is used verbatim; no CWD-relative resolution happens
/// here, and none should. `SumStore` has never consulted the process
/// working directory, so a relative path is resolved by the operating
/// system against whatever CWD the process happens to hold — which is
/// exactly the ambiguity the flag exists to remove.
pub fn resolve_store_dir(flag: Option<PathBuf>) -> Result<PathBuf, SumError> {
    if let Some(dir) = flag {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os(STORE_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| SumError::Config(NO_ROOT_MESSAGE.to_owned()))?;
    Ok(PathBuf::from(home).join(".sumnode").join("store"))
}

impl StoreConfig {
    /// Build a store config with the root resolved by
    /// [`resolve_store_dir`] and the default message cap.
    ///
    /// There is no `Default` impl, and there must not be one: `Default` cannot
    /// fail, so every in-place fix for the removed `/tmp/sumnode` fallback
    /// needs a sentinel, and every sentinel is either CWD-relative or a panic
    /// inside a `Default` impl. Callers name a root or handle the error.
    pub fn resolve(store_dir_flag: Option<PathBuf>) -> Result<Self, SumError> {
        Ok(Self {
            store_dir: resolve_store_dir(store_dir_flag)?,
            max_chunk_msg_bytes: DEFAULT_MAX_CHUNK_MSG_BYTES,
        })
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
    fn store_config_carries_the_resolved_root_and_the_default_cap() {
        // Takes the same lock as the resolution tests below: this reads $HOME
        // through `resolve`, and they mutate it. Without the lock this test
        // passes alone and flakes under `cargo test`'s default threading.
        // (Was `store_config_defaults`, which went through the removed
        // `Default` impl.)
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        let cfg = StoreConfig::resolve(None).expect("a root is nameable");
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
            resolve_store_dir(Some(PathBuf::from("/from/flag"))).unwrap(),
            PathBuf::from("/from/flag")
        );
    }

    #[test]
    fn the_env_wins_over_home_when_no_flag_is_given() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, Some("/from/env"));
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(resolve_store_dir(None).unwrap(), PathBuf::from("/from/env"));
    }

    #[test]
    fn home_is_the_fallback_and_the_path_is_the_documented_one() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", Some("/from/home"));

        assert_eq!(
            resolve_store_dir(None).unwrap(),
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
            resolve_store_dir(None).unwrap(),
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

        let from_here = resolve_store_dir(None).unwrap();
        let cwd = std::env::current_dir().expect("a working directory");
        assert!(
            !from_here.starts_with(&cwd) || cwd == std::path::Path::new("/from/home"),
            "resolution must not be relative to {}",
            cwd.display()
        );
    }

    /// With no flag, no `SUM_STORE_DIR` and no `HOME`, resolution fails and the
    /// error names the flag the operator can act on.
    ///
    /// This is the whole of step 3: the previous code answered this case with
    /// `/tmp/sumnode` — one fixed path, shared by every user and every node on
    /// the host, chosen silently and precisely when the environment was least
    /// well understood. Two nodes landing there share one chunk namespace and
    /// one `manifests/` directory, each garbage-collecting against its own
    /// assignment set.
    #[test]
    fn resolution_fails_closed_when_no_source_names_a_root() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", None);

        let err = resolve_store_dir(None)
            .expect_err("no source names a root, so resolution must not invent one");
        let msg = err.to_string();
        assert!(
            msg.contains("--store-dir"),
            "the error must name the flag the operator can act on: {msg}"
        );
        assert!(
            !msg.contains("/tmp/sumnode"),
            "the shared-path fallback must be gone, not merely mentioned: {msg}"
        );
    }

    /// An empty `HOME` is not a home directory. Accepting it resolves every
    /// node to `/.sumnode/store` — the same class of shared path the
    /// `/tmp/sumnode` fallback was.
    #[test]
    fn an_empty_home_is_not_a_root_either() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", Some(""));

        assert!(
            resolve_store_dir(None).is_err(),
            "an empty HOME must not resolve to the filesystem root"
        );
    }

    /// An explicit root is still honoured with nothing else in the
    /// environment: fail-closed applies to the absence of every source, not to
    /// the absence of `HOME`.
    #[test]
    fn an_explicit_root_still_resolves_with_an_empty_environment() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(STORE_DIR_ENV, None);
        let _home = EnvGuard::set("HOME", None);

        assert_eq!(
            resolve_store_dir(Some(PathBuf::from("/from/flag"))).unwrap(),
            PathBuf::from("/from/flag")
        );
    }
}
