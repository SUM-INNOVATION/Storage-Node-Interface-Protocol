//! SNIP V2 cryptographic primitives.
//!
//! This crate is **pure functions, no I/O, no networking, no async**. It
//! exists to keep the cryptography surface small and auditable.
//!
//! # What's here
//!
//! * Per-chunk encryption ([`encrypt_chunk`] / [`decrypt_chunk`]) using
//!   ChaCha20-Poly1305 with per-chunk keys and nonces derived via
//!   HKDF-SHA256 from a single 32-byte file master key (`K_file`).
//! * Manifest encryption ([`encrypt_manifest`] / [`decrypt_manifest`])
//!   using a separate HKDF-derived key and a fixed all-zero nonce — safe
//!   because the manifest key is unique per file and only used once per
//!   file.
//! * Per-recipient key wrap ([`wrap_for_recipient`] /
//!   [`unwrap_for_self`]) using X25519 ECDH + ChaCha20-Poly1305. Each
//!   bundle is a fixed 80 bytes: `eph_pub (32) || ciphertext (32) ||
//!   tag (16)`.
//!
//! # What's NOT here
//!
//! * Anything that talks to libp2p, the L1 RPC, the local store.
//! * Anything that decides policy (which recipients, which expiry).
//! * Network identity (Ed25519 keypair generation, peer-id derivation).
//!   The encryption keys here are X25519 — distinct from the Ed25519
//!   signing keys used elsewhere in SNIP, on purpose.
//!
//! # Wire-format constants
//!
//! These are stable across the protocol; changing them requires a
//! versioned crypto-suite negotiation:
//!
//! * `ChaCha20-Poly1305` (RFC 8439): 32-byte key, 12-byte nonce, 16-byte tag.
//! * `X25519` (RFC 7748): 32-byte private scalar (clamped before
//!   scalar-multiplication), 32-byte public key (Montgomery
//!   U-coordinate of the scalar's basepoint multiple). Note: the
//!   *private scalar* is what gets clamped per RFC 7748; public keys
//!   are simply the result of `scalar_mult_base(clamped_scalar)`.
//! * `HKDF-SHA256` (RFC 5869) with explicit `salt` and `info` per use.

#![forbid(unsafe_code)]

mod chunk;
mod errors;
mod kdf;
mod manifest;
mod recipient;

pub use chunk::{decrypt_chunk, encrypt_chunk};
pub use errors::CryptoError;
pub use manifest::{decrypt_manifest, encrypt_manifest};
pub use recipient::{
    unwrap_for_self, wrap_for_recipient, x25519_keypair_from_ed25519_seed,
    RECIPIENT_BUNDLE_SIZE,
};

/// Length of the AEAD authentication tag in bytes (Poly1305 = 16).
pub const TAG_SIZE: usize = 16;

/// Length of the file master key (`K_file`) in bytes.
pub const K_FILE_SIZE: usize = 32;

/// Length of an X25519 public or private key in bytes.
pub const X25519_KEY_SIZE: usize = 32;
