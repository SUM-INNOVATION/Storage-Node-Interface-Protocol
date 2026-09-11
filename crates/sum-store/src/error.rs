use std::io;

/// Crate-local error type for `sum-store` operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Native Windows (and any other non-Unix target) is fail-closed for store
    /// publication: the confined-open, no-clobber link and directory-durability
    /// guarantees have no validated equivalent there. Tracked in SNIP #49.
    #[error(
        "store publication is not supported on this platform: native Windows is \
         unsupported; run under WSL2 or another Unix target"
    )]
    UnsupportedPlatform,

    /// The exclusive store lease has no validated non-Unix equivalent: it is
    /// `flock(LOCK_EX | LOCK_NB)` held on an open file description, and the
    /// Windows analogues differ in inheritance and in release-on-crash
    /// semantics. Same fail-closed posture as `UnsupportedPlatform`.
    #[error(
        "the exclusive store lease is not supported on this platform: native \
         Windows is unsupported; run under WSL2 or another Unix target"
    )]
    LeaseUnsupportedPlatform,

    /// Another process (or another handle in this one) holds the store root.
    #[error(
        "store root {root} is already in use: an exclusive lease on {lease} is \
         held elsewhere. Two nodes sharing a root share one chunk namespace \
         and garbage-collect against each other; give this node its own root \
         with --store-dir"
    )]
    StoreRootBusy { root: String, lease: String },

    #[error("invalid store key: {0}")]
    InvalidKey(String),

    #[error("publication requires canonical CIDv1/raw/BLAKE3-256: {0}")]
    InvalidCid(String),

    #[error("publication may exist for {cid}, but durability is unconfirmed: {detail}")]
    DurabilityUnconfirmed { cid: String, detail: String },

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("integrity check failed: expected {expected}, got {actual}")]
    IntegrityMismatch { expected: String, actual: String },

    #[error("chunk not found: {0}")]
    NotFound(String),

    #[error("merkle error: {0}")]
    Merkle(String),

    #[error("manifest not found for merkle root: {0}")]
    ManifestNotFound(String),

    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout this crate.
pub type Result<T> = std::result::Result<T, StoreError>;
