// Global error type for the SUM Storage Node.

#[derive(Debug, thiserror::Error)]
pub enum SumError {
    #[error("network error: {0}")]
    Network(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("merkle error: {0}")]
    Merkle(String),

    #[error("identity error: {0}")]
    Identity(String),
}
