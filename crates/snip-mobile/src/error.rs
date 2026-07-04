//! FFI error taxonomy. Coarser than the internal error types on purpose:
//! the wallet needs to distinguish user-actionable failures (no peers,
//! insufficient balance) from retryable and fatal ones, not the full
//! pipeline stage detail — that travels in the message string.

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SnipError {
    #[error("invalid configuration: {msg}")]
    InvalidConfig { msg: String },
    #[error("i/o error: {msg}")]
    Io { msg: String },
    #[error("chain RPC error: {msg}")]
    Rpc { msg: String },
    #[error("SNIP V2 is not enabled on this chain")]
    V2NotEnabled,
    #[error("transaction rejected: {reason}")]
    TxRejected { reason: String },
    #[error("timed out waiting for transaction finality")]
    TxTimeout,
    #[error("no storage peers reachable — check bootstrap peers and connectivity")]
    NoPeersReachable,
    #[error("upload incomplete: {chunks_missing} chunk(s) not yet replicated ({detail})")]
    PushIncomplete { chunks_missing: u32, detail: String },
    #[error("timed out waiting for chunk coverage before activation")]
    CoverageTimeout,
    #[error("file not found on chain")]
    NotFound,
    #[error("file is not active on chain (lifecycle: {lifecycle})")]
    NotActive { lifecycle: String },
    #[error("private files are not supported yet")]
    PrivateUnsupported,
    #[error("content integrity verification failed: {msg}")]
    IntegrityFailure { msg: String },
    #[error("operation cancelled")]
    Cancelled,
    #[error("internal error: {msg}")]
    Internal { msg: String },
}

impl SnipError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        SnipError::Internal { msg: e.to_string() }
    }

    pub fn rpc(e: impl std::fmt::Display) -> Self {
        SnipError::Rpc { msg: e.to_string() }
    }

    pub fn io(e: impl std::fmt::Display) -> Self {
        SnipError::Io { msg: e.to_string() }
    }
}
