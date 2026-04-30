//! Error types surfaced by `sum-crypto`.
//!
//! Decryption errors deliberately do not include the underlying AEAD
//! reason — exposing a more detailed reason can leak side-channel
//! information ("invalid tag" vs "wrong key" should be indistinguishable
//! to a passive observer of the API).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    /// The caller passed a buffer with an unexpected length. The crate
    /// uses fixed-size byte arrays for keys and bundles, but chunk
    /// payloads are variable — this variant is only used when a fixed-
    /// size precondition is violated (e.g. unwrap bundle != 80 bytes,
    /// `K_file` != 32 bytes).
    #[error("invalid input length: expected {expected}, got {got}")]
    InvalidLength { expected: usize, got: usize },

    /// AEAD authentication failed. The ciphertext is either tampered
    /// with, the wrong key was used, or the AAD differs from the
    /// encryption side. Intentionally opaque.
    #[error("decryption failed (authentication tag mismatch)")]
    DecryptionFailed,

    /// The X25519 ECDH produced an all-zero shared secret. This happens
    /// when one party's public key is a low-order point: the resulting
    /// shared secret is independent of the other party's private key,
    /// so a `K_file` wrapped against such a key would be encrypted under
    /// a predictable KEK derivable by anyone.
    ///
    /// Refuse both wrap (so no `K_file` ever leaks via this path) and
    /// unwrap (defense in depth — a malicious owner could plant such a
    /// bundle and we don't want to derive any KEK material from it).
    /// Chain-side `RegisterEncryptionKey` should also reject low-order
    /// pubkeys as a separate layer.
    #[error("non-contributory X25519 key (low-order point detected)")]
    NonContributoryKey,
}
