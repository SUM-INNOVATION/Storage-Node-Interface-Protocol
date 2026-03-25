use std::io;

/// Crate-local error type for `sum-store` operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("integrity check failed: expected {expected}, got {actual}")]
    IntegrityMismatch { expected: String, actual: String },

    #[error("chunk not found: {0}")]
    NotFound(String),

    #[error("merkle error: {0}")]
    Merkle(String),

    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout this crate.
pub type Result<T> = std::result::Result<T, StoreError>;
