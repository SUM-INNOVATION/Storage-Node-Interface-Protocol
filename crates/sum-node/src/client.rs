//! Library interface for ingest and download (SNIP #57, item 1).
//!
//! The supported way to upload and fetch files from Rust instead of through
//! the `sum-node` binary. It is a thin layer over the code the CLI runs, so
//! the protocol, the chain transactions and the file format are the ones
//! `sum-node ingest-v2` and `sum-node download` use:
//!
//! * **Ingest.** [`IngestPipeline::ingest`] and
//!   [`IngestPipeline::ingest_private`] take a file path or an in-memory
//!   byte slice ([`IngestInput`]) and run the V2 pipeline
//!   ([`IngestPipeline::run`] / [`IngestPipeline::run_private`]). They
//!   return its [`IngestOutcome`]; [`IngestOutcome::manifest`] and
//!   [`IngestOutcome::merkle_root`] read the manifest and the root from it.
//! * **Download.** [`download_to_path`] and [`download_to_bytes`] fetch a
//!   file by merkle root, routed by the chain's V2 row exactly as
//!   `sum-node download` routes it, and return a [`Downloaded`] report that
//!   carries the manifest.
//!
//! Byte-slice ingest and byte download are staged through a temporary file,
//! because the pipeline memory-maps its input and the download orchestrators
//! write their output to a path. The guide is `docs/client/library.md`.
//!
//! ```no_run
//! use std::sync::Arc;
//! use sum_node::client::{
//!     DownloadConfig, IngestInput, IngestOutcome, IngestParams, IngestParamsDefaults,
//!     IngestPipeline, L1RpcClient, MapPeerResolver, NetConfig, SumNet, download_to_bytes,
//!     identity,
//! };
//!
//! # async fn example(seed: [u8; 32], episode: Vec<u8>) -> anyhow::Result<()> {
//! let rpc = Arc::new(L1RpcClient::new("https://rpc.example".to_string()));
//! let params = IngestParams::from_chain_params(
//!     &rpc.chain_get_chain_params().await?,
//!     IngestParamsDefaults::default(),
//! );
//! let keypair = identity::keypair_from_seed(&seed)?;
//! let my_addr = identity::l1_address_from_keypair(&keypair);
//! let net = Arc::new(SumNet::new(NetConfig::default(), keypair).await?);
//! let pipeline = IngestPipeline::new(
//!     rpc.clone(),
//!     net.clone(),
//!     Arc::new(MapPeerResolver::new()),
//!     seed,
//!     my_addr,
//!     params,
//! );
//!
//! let outcome = pipeline
//!     .ingest(IngestInput::Bytes { file_name: "episode-0017.bin", data: &episode })
//!     .await;
//! net.shutdown().await?;
//! let root = outcome.merkle_root().expect("chunked");
//! assert!(matches!(outcome, IngestOutcome::Activated { .. }), "{outcome:?}");
//!
//! let config = DownloadConfig::with_seed(rpc, NetConfig::default(), seed)?;
//! let (bytes, report) = download_to_bytes(&config, root).await?;
//! assert_eq!(bytes, episode);
//! assert_eq!(report.manifest.file_name, "episode-0017.bin");
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sum_store::SumStore;
use sum_types::config::StoreConfig;
use tokio::sync::RwLock;

use crate::download::DownloadOrchestrator;
use crate::download_private::download_private_with_manifest;
use crate::download_route::route_download_target;
use crate::ingest_v2::{PeerResolver, V2IngestNet, V2IngestRpc};
use crate::runtime_params::RuntimeChainParams;

pub use crate::download::DownloadResult;
pub use crate::download_route::DownloadPath;
pub use crate::ingest_v2::{
    IngestOutcome, IngestParams, IngestParamsDefaults, IngestPipeline, IngestStage,
    MapPeerResolver, PrivateIngestSpec, PrivateRecipient, SuggestedAction,
};
pub use crate::rpc_client::L1RpcClient;
pub use sum_net::identity;
pub use sum_net::{Keypair, SumNet};
pub use sum_types::config::NetConfig;
pub use sum_types::storage::DataManifest;

// ── Ingest ──────────────────────────────────────────────────────────────────

/// What to ingest: a file on disk or bytes in memory.
#[derive(Clone, Copy)]
pub enum IngestInput<'a> {
    /// A file on disk. The pipeline memory-maps it in place; the manifest's
    /// `file_name` is the path's last component.
    Path(&'a Path),
    /// Bytes in memory. They are written as `file_name` to a new temporary
    /// directory under `TMPDIR` (on Unix, readable only by the current
    /// user), ingested from there, and removed when the call returns,
    /// whatever the outcome. The manifest, and therefore the merkle root,
    /// is identical to ingesting a file named `file_name` with the same
    /// contents.
    Bytes {
        /// The manifest's `file_name`. Must be a single path component: not
        /// empty, no `/`, not `.` or `..`.
        file_name: &'a str,
        /// The file contents.
        data: &'a [u8],
    },
}

/// Prints the length of in-memory data, not the bytes.
impl std::fmt::Debug for IngestInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestInput::Path(path) => f.debug_tuple("Path").field(path).finish(),
            IngestInput::Bytes { file_name, data } => f
                .debug_struct("Bytes")
                .field("file_name", file_name)
                .field("len", &data.len())
                .finish(),
        }
    }
}

impl<R, N, P> IngestPipeline<R, N, P>
where
    R: V2IngestRpc + 'static,
    N: V2IngestNet + 'static,
    P: PeerResolver + 'static,
{
    /// Ingest a Public file from a path or from bytes: the same S0–S6
    /// pipeline as [`Self::run`] (and `sum-node ingest-v2 --visibility
    /// public`).
    ///
    /// The outcome carries the manifest and the merkle root whenever the
    /// input was chunked ([`IngestOutcome::manifest`],
    /// [`IngestOutcome::merkle_root`]). If `Bytes` cannot be staged (an
    /// invalid `file_name`, or the temporary file cannot be written), the
    /// outcome is [`IngestOutcome::Failed`] at [`IngestStage::Register`]
    /// with no manifest; nothing is sent to the chain.
    ///
    /// A [`IngestOutcome::PendingNeedsAction`] outcome is finished with
    /// [`Self::resume`], which takes a path, or [`Self::abandon`]. To
    /// resume a byte-slice ingest, write the same bytes to a file named
    /// `file_name` and pass that path.
    pub async fn ingest(&self, input: IngestInput<'_>) -> IngestOutcome {
        match input {
            IngestInput::Path(path) => self.run(path).await,
            IngestInput::Bytes { file_name, data } => match StagedBytes::write(file_name, data) {
                Ok(staged) => self.run(&staged.path).await,
                Err(source) => staging_failed(source),
            },
        }
    }

    /// Ingest a Private file from a path or from bytes: the same pipeline
    /// as [`Self::run_private`] (and `sum-node ingest-v2 --visibility
    /// private`). The file is encrypted under a fresh key, so the merkle
    /// root (of the ciphertext) differs on every call and cannot be known
    /// before this returns.
    ///
    /// With `Bytes`, the **plaintext** is written to the temporary file
    /// (on Unix in a mode-0700 directory, as a mode-0600 file) for the
    /// duration of the call. Staging failures are reported as in
    /// [`Self::ingest`].
    pub async fn ingest_private(
        &self,
        input: IngestInput<'_>,
        spec: PrivateIngestSpec,
    ) -> IngestOutcome {
        match input {
            IngestInput::Path(path) => self.run_private(path, spec).await,
            IngestInput::Bytes { file_name, data } => match StagedBytes::write(file_name, data) {
                Ok(staged) => self.run_private(&staged.path, spec).await,
                Err(source) => staging_failed(source),
            },
        }
    }
}

/// In-memory input written to a temporary file. The directory, and the file
/// in it, are removed when this is dropped.
struct StagedBytes {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl StagedBytes {
    fn write(file_name: &str, data: &[u8]) -> Result<Self> {
        check_file_name(file_name)?;
        let dir = private_tempdir("sum-node-ingest-")
            .context("ingest: create a temporary directory for the input bytes")?;
        let path = dir.path().join(file_name);
        write_private_file(&path, data)
            .with_context(|| format!("ingest: write the input bytes to {}", path.display()))?;
        Ok(Self { path, _dir: dir })
    }
}

/// A new temporary directory under `TMPDIR` that, on Unix, only the current
/// user can open (mode 0700). `tempfile` would create it with the default
/// mode, usually world-readable, and staged input may be Private plaintext.
fn private_tempdir(prefix: &str) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

/// Create `path` (which must not exist) readable only by the current user
/// on Unix (mode 0600), and write `data` to it.
fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

/// `file_name` becomes a path component inside the staging directory, so it
/// must be exactly one normal component: anything else could name a file
/// outside it, or change the manifest's `file_name`.
fn check_file_name(file_name: &str) -> Result<()> {
    let mut components = Path::new(file_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == OsStr::new(file_name) => Ok(()),
        _ => bail!(
            "ingest: file_name {file_name:?} is not a plain file name \
             (it must be one path component: not empty, no `/`, not `.` or `..`)"
        ),
    }
}

/// The outcome for input that could not be staged. Same shape as an S0
/// failure inside the pipeline: nothing was chunked or sent.
fn staging_failed(source: anyhow::Error) -> IngestOutcome {
    IngestOutcome::Failed {
        stage: IngestStage::Register,
        manifest: None,
        source,
    }
}

// ── Download ────────────────────────────────────────────────────────────────

/// Settings for [`download_to_path`] and [`download_to_bytes`]: the chain
/// endpoint, the network, the identity, and the knobs `sum-node download`
/// takes as flags.
///
/// The identity is set by the constructor so that the network key and the
/// key used to decrypt Private files always belong to the same account.
#[derive(Clone)]
pub struct DownloadConfig {
    /// Chain JSON-RPC client. Used for the file's V2 row, chain parameters,
    /// the assignment snapshot and (Private files) the access list.
    pub rpc: Arc<L1RpcClient>,
    /// Network settings for the per-call [`SumNet`].
    pub net_config: NetConfig,
    /// Local chunk store root for Public and legacy downloads, resolved
    /// like `--store-dir` (`None`: `SUM_STORE_DIR`, then
    /// `$HOME/.sumnode/store`). The download holds the store's lease for
    /// the call and leaves fetched chunks in it. Unused for Private files.
    pub store_dir: Option<PathBuf>,
    /// Maximum chunk requests in flight (`--max-concurrent`, default 10).
    pub max_concurrent: usize,
    /// Overall download deadline (`--download-timeout-secs`, default 300 s).
    pub timeout: Duration,
    keypair: Keypair,
    seed: Option<[u8; 32]>,
}

impl DownloadConfig {
    /// Download as the account of `seed`, the 32-byte Ed25519 seed of a
    /// `--key-file`. Required for Private files; works for every file.
    pub fn with_seed(rpc: Arc<L1RpcClient>, net_config: NetConfig, seed: [u8; 32]) -> Result<Self> {
        let keypair = sum_net::identity::keypair_from_seed(&seed)?;
        Ok(Self::build(rpc, net_config, keypair, Some(seed)))
    }

    /// Download under a fresh random network identity, like `sum-node
    /// download` without `--key-file`. Public and legacy files only: a
    /// Private file is refused before any network activity.
    pub fn ephemeral(rpc: Arc<L1RpcClient>, net_config: NetConfig) -> Self {
        Self::build(rpc, net_config, Keypair::generate_ed25519(), None)
    }

    fn build(
        rpc: Arc<L1RpcClient>,
        net_config: NetConfig,
        keypair: Keypair,
        seed: Option<[u8; 32]>,
    ) -> Self {
        Self {
            rpc,
            net_config,
            store_dir: None,
            max_concurrent: 10,
            timeout: Duration::from_secs(300),
            keypair,
            seed,
        }
    }
}

/// Report of a completed download.
#[derive(Debug, Clone)]
pub struct Downloaded {
    /// The merkle root that was downloaded.
    pub merkle_root: [u8; 32],
    /// The path the chain's V2 row selected: Public, Private, or legacy (no
    /// V2 row).
    pub route: DownloadPath,
    /// The manifest the output was assembled from, checked against
    /// `merkle_root` when it was received. For a Private file it is the
    /// decrypted manifest: `merkle_root` is the root of the ciphertext,
    /// `file_hash` and `total_size_bytes` describe the plaintext. Its
    /// `file_name` is the name the owner gave at ingest.
    pub manifest: DataManifest,
    /// Transfer statistics. `None` for Private files, whose download path
    /// does not collect them.
    pub stats: Option<DownloadResult>,
}

/// Download the file with `merkle_root` and write it to `output`, routed
/// and verified exactly as `sum-node download`:
///
/// 1. Read the file's V2 row (`storage_getFileInfoV2`) before any network
///    activity. A Private file without a seed is refused here. As in the
///    CLI, a failed lookup counts as "no V2 row" and takes the legacy path.
/// 2. Route on the row's visibility alone: Public, Private, or legacy;
///    an unknown visibility is refused.
/// 3. Start a [`SumNet`] for this call, run the CLI's orchestrator for the
///    route (Public and legacy through the local chunk store, Private
///    decrypted with the account's key), and shut the network down. The
///    checks are the CLI's: the manifest against the root, chunks against
///    the manifest, and for Private files the plaintext hashes.
///
/// `output` is created or overwritten. On error it may hold a partial file.
pub async fn download_to_path(
    config: &DownloadConfig,
    merkle_root: [u8; 32],
    output: &Path,
) -> Result<Downloaded> {
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let v2_info = config
        .rpc
        .storage_get_file_info_v2(&root_hex, None, None)
        .await
        .ok()
        .flatten();
    if let Some(info) = v2_info.as_ref()
        && info.visibility.is_private()
        && config.seed.is_none()
    {
        bail!(
            "download of Private file {root_hex} requires a seed \
             (DownloadConfig::with_seed): no X25519 secret to unwrap K_file"
        );
    }
    let route = route_download_target(v2_info.as_ref())?;

    let net = Arc::new(SumNet::new(config.net_config.clone(), config.keypair.clone()).await?);
    let fetched = fetch_routed(config, &net, route, v2_info, merkle_root, output).await;
    // The Private path shuts the network down itself on success; a second
    // shutdown only reports that the swarm has already stopped.
    net.shutdown().await.ok();
    let (manifest, stats) = fetched?;
    Ok(Downloaded {
        merkle_root,
        route,
        manifest,
        stats,
    })
}

/// Download the file with `merkle_root` into memory: [`download_to_path`]
/// into a private temporary directory, then read back. The whole file is
/// held in memory, and for a Private file the plaintext is on disk in that
/// directory until this returns. Prefer [`download_to_path`] for large
/// files.
pub async fn download_to_bytes(
    config: &DownloadConfig,
    merkle_root: [u8; 32],
) -> Result<(Vec<u8>, Downloaded)> {
    read_back_staged(|path| async move { download_to_path(config, merkle_root, &path).await }).await
}

/// Run `download` with a path in a new temporary directory, then read the
/// file it wrote. The directory is removed on return, on success or error.
async fn read_back_staged<T, F, Fut>(download: F) -> Result<(Vec<u8>, T)>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let dir = private_tempdir("sum-node-download-")
        .context("download: create a temporary directory for the output")?;
    let path = dir.path().join("download");
    let report = download(path.clone()).await?;
    let data =
        std::fs::read(&path).with_context(|| format!("download: read back {}", path.display()))?;
    Ok((data, report))
}

/// Step 3 of [`download_to_path`] on a running network. Mirrors the
/// dispatch in `sum-node download`.
async fn fetch_routed(
    config: &DownloadConfig,
    net: &Arc<SumNet>,
    route: DownloadPath,
    v2_info: Option<sum_types::rpc_types::StorageFileInfoV2>,
    merkle_root: [u8; 32],
    output: &Path,
) -> Result<(DataManifest, Option<DownloadResult>)> {
    if route == DownloadPath::V2Private {
        let info = v2_info.ok_or_else(|| anyhow!("Private route without a V2 row"))?;
        let seed = config
            .seed
            .ok_or_else(|| anyhow!("Private route without a seed"))?;
        let manifest = download_private_with_manifest(
            config.keypair.clone(),
            seed,
            config.rpc.clone(),
            net.clone(),
            info,
            merkle_root,
            output.to_path_buf(),
            config.max_concurrent,
            config.timeout,
        )
        .await?;
        return Ok((manifest, None));
    }

    let store = Arc::new(RwLock::new(SumStore::new(StoreConfig::resolve(
        config.store_dir.clone(),
    )?)?));
    let peer_addresses = Arc::new(RwLock::new(HashMap::new()));
    // Same fail-closed rule as the CLI: the legacy holder map needs the live
    // replication factor, never a hardcoded one.
    let replication_factor = match config.rpc.chain_get_chain_params().await {
        Ok(cp) => RuntimeChainParams::from_chain(&cp).assignment_replication_factor,
        Err(e) => bail!(
            "download: chain_getChainParams failed; refusing to build V1 holder \
             map with a hardcoded replication factor. RPC error: {e}"
        ),
    };
    // Bare lowercase hex: the form archives accept in a legacy manifest
    // request.
    let orchestrator = DownloadOrchestrator::new(
        hex::encode(merkle_root),
        output.to_path_buf(),
        config.rpc.clone(),
        config.max_concurrent,
        config.timeout,
        replication_factor,
    );
    let (stats, manifest) = match (route, v2_info) {
        (DownloadPath::V2Public, Some(info)) => {
            orchestrator
                .run_v2_public_with_manifest(net.clone(), store, peer_addresses, info)
                .await?
        }
        (DownloadPath::V1Legacy, _) => {
            orchestrator
                .run_with_manifest(net.clone(), store, peer_addresses)
                .await?
        }
        (route, _) => bail!("download route {route:?} does not match the chain row"),
    };
    Ok((manifest, Some(stats)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rpc_server::{MockResponse, routes, start_mock_rpc};
    use serde_json::json;

    #[test]
    fn file_name_must_be_one_plain_component() {
        for ok in ["episode-0017.bin", "a", ".hidden", "name with spaces"] {
            assert!(check_file_name(ok).is_ok(), "{ok:?} should be accepted");
        }
        for bad in ["", ".", "..", "a/b", "a/", "/a", "./a", "../a", "/"] {
            assert!(check_file_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn staged_bytes_are_named_as_given_and_removed_on_drop() {
        let staged = StagedBytes::write("episode-0017.bin", b"payload").unwrap();
        assert_eq!(
            staged.path.file_name(),
            Some(OsStr::new("episode-0017.bin"))
        );
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"payload");
        let dir = staged.path.parent().unwrap().to_path_buf();
        drop(staged);
        assert!(!dir.exists(), "staging directory must be removed");
    }

    #[cfg(unix)]
    #[test]
    fn staged_bytes_are_readable_only_by_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let staged = StagedBytes::write("diary.txt", b"plaintext").unwrap();
        let dir = staged.path.parent().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir), 0o700, "staging directory");
        assert_eq!(mode(&staged.path), 0o600, "staged file");
    }

    #[test]
    fn ingest_input_debug_omits_the_bytes() {
        let data = vec![0xABu8; 4096];
        let shown = format!(
            "{:?}",
            IngestInput::Bytes {
                file_name: "f.bin",
                data: &data
            }
        );
        assert_eq!(shown, r#"Bytes { file_name: "f.bin", len: 4096 }"#);
    }

    #[tokio::test]
    async fn download_to_bytes_returns_what_the_download_wrote_and_cleans_up() {
        let written = vec![0x5Au8; 3 * 1024 * 1024 + 7];
        let mut staged_at = None;
        let (data, report) = read_back_staged(|path| {
            staged_at = Some(path.clone());
            let written = written.clone();
            async move {
                std::fs::write(&path, &written)?;
                Ok("report")
            }
        })
        .await
        .unwrap();
        assert_eq!(data, written);
        assert_eq!(report, "report");
        let dir = staged_at.unwrap().parent().unwrap().to_path_buf();
        assert!(!dir.exists(), "staging directory must be removed");
    }

    #[tokio::test]
    async fn download_to_bytes_returns_the_download_error_and_cleans_up() {
        let mut staged_at = None;
        let err = read_back_staged(|path| {
            staged_at = Some(path.clone());
            async move {
                // A partial write, then failure: the partial file must not
                // be returned as data.
                std::fs::write(&path, b"partial")?;
                Err::<(), _>(anyhow!("timeout: downloaded 1/4 chunks before deadline"))
            }
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("1/4 chunks"), "{err}");
        let dir = staged_at.unwrap().parent().unwrap().to_path_buf();
        assert!(!dir.exists(), "staging directory must be removed");
    }

    fn file_row(root: [u8; 32], visibility: u8) -> serde_json::Value {
        json!({
            "merkle_root": format!("0x{}", hex::encode(root)),
            "owner": "OwnerB58Addr",
            "plaintext_size_bytes": 1024,
            "stored_size_bytes": 1040,
            "chunk_count": 1,
            "fee_pool": 1000,
            "created_at": 10,
            "activated_at_height": 20,
            "assignment_height": 10,
            "visibility": visibility,
            "lifecycle": 1,
            "access_list": []
        })
    }

    /// A Private file without a seed is refused after the one chain lookup,
    /// before any network or store is touched.
    #[tokio::test]
    async fn download_private_file_without_seed_is_refused_before_networking() {
        let root = [0x22u8; 32];
        let server = start_mock_rpc(routes([(
            "storage_getFileInfoV2",
            MockResponse::Result(file_row(root, 1)),
        )]))
        .await;
        let config = DownloadConfig::ephemeral(
            Arc::new(L1RpcClient::new(server.url())),
            NetConfig::default(),
        );
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("out.bin");

        let err = download_to_path(&config, root, &output).await.unwrap_err();
        assert!(err.to_string().contains("requires a seed"), "{err}");
        assert_eq!(server.requests().len(), 1, "only the V2 row lookup");
        assert_eq!(server.method_count("storage_getFileInfoV2"), 1);
        assert!(!output.exists(), "nothing written");

        let err = download_to_bytes(&config, root).await.unwrap_err();
        assert!(err.to_string().contains("requires a seed"), "{err}");
    }

    /// An unknown visibility byte is refused (fail closed), not downloaded
    /// as Public.
    #[tokio::test]
    async fn download_unknown_visibility_is_refused() {
        let root = [0x33u8; 32];
        let server = start_mock_rpc(routes([(
            "storage_getFileInfoV2",
            MockResponse::Result(file_row(root, 7)),
        )]))
        .await;
        let config = DownloadConfig::ephemeral(
            Arc::new(L1RpcClient::new(server.url())),
            NetConfig::default(),
        );
        let out = tempfile::tempdir().unwrap();
        let err = download_to_path(&config, root, &out.path().join("out.bin"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("visibility = 7"), "{err}");
        assert_eq!(server.requests().len(), 1, "only the V2 row lookup");
    }
}
