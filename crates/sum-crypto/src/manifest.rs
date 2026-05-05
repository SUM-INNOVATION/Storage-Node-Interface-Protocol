//! Encrypted manifest.
//!
//! For Private files, the manifest itself is encrypted so storage nodes
//! can't read `file_name`, `mime_type`, or per-chunk metadata. The chunk
//! list is part of the encrypted blob; storage nodes only see opaque
//! ciphertext under the CID `"manifest:<merkle_root>"`.
//!
//! Construction:
//!
//! ```text
//! manifest_key   = HKDF-SHA256(salt = b"", ikm = K_file, info = "snip-manifest-key-v1")
//! manifest_nonce = [0; 12]                          // safe: manifest_key is unique per file
//! ct, tag        = ChaCha20-Poly1305-Encrypt(plaintext = manifest_bytes,
//!                                             key   = manifest_key,
//!                                             nonce = manifest_nonce,
//!                                             aad   = b"snip-manifest-v1")
//! stored = ct || tag
//! ```
//!
//! The fixed all-zero nonce is intentional and safe: each file generates
//! a fresh `K_file` (32 random bytes), which derives a fresh
//! `manifest_key`, which is used to encrypt **exactly one** plaintext —
//! the manifest. Nonce-reuse only matters when the same key encrypts
//! multiple messages, which doesn't happen here.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};

use crate::errors::CryptoError;
use crate::kdf::{MANIFEST_KEY_INFO, hkdf_expand};

/// Domain-separation tag for manifest AEAD AAD. Distinct from chunk AAD.
const MANIFEST_AAD: &[u8] = b"snip-manifest-v1";

/// All-zero 12-byte nonce. Safe because `manifest_key` is freshly
/// derived per file and used exactly once.
const ZERO_NONCE: [u8; 12] = [0u8; 12];

fn manifest_key(k_file: &[u8; 32]) -> [u8; 32] {
    hkdf_expand::<32>(b"", k_file, MANIFEST_KEY_INFO)
}

/// Encrypt a serialized manifest for storage.
pub fn encrypt_manifest(k_file: &[u8; 32], manifest_bytes: &[u8]) -> Vec<u8> {
    let mk = manifest_key(k_file);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&mk));
    cipher
        .encrypt(
            Nonce::from_slice(&ZERO_NONCE),
            Payload {
                msg: manifest_bytes,
                aad: MANIFEST_AAD,
            },
        )
        .expect("ChaCha20-Poly1305 manifest encrypt cannot fail for normal manifest sizes")
}

/// Decrypt a stored manifest blob.
pub fn decrypt_manifest(k_file: &[u8; 32], stored: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mk = manifest_key(k_file);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&mk));
    cipher
        .decrypt(
            Nonce::from_slice(&ZERO_NONCE),
            Payload {
                msg: stored,
                aad: MANIFEST_AAD,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TAG_SIZE;

    fn fixed_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(0x42);
        }
        k
    }

    #[test]
    fn roundtrip_manifest() {
        let k = fixed_key();
        let bytes = b"manifest bytes (CBOR/bincode would be here)";
        let stored = encrypt_manifest(&k, bytes);
        assert_eq!(stored.len(), bytes.len() + TAG_SIZE);
        let recovered = decrypt_manifest(&k, &stored).unwrap();
        assert_eq!(recovered, bytes);
    }

    #[test]
    fn wrong_k_file_rejects_manifest() {
        let k_a = fixed_key();
        let mut k_b = k_a;
        k_b[5] ^= 0x80;
        let stored = encrypt_manifest(&k_a, b"secret manifest");
        assert!(matches!(
            decrypt_manifest(&k_b, &stored),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn tampered_manifest_rejected() {
        let k = fixed_key();
        let mut stored = encrypt_manifest(&k, b"manifest");
        stored[0] ^= 0x01;
        assert!(matches!(
            decrypt_manifest(&k, &stored),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn manifest_key_distinct_from_chunk_keys() {
        // Tampering with the manifest under chunk AAD must fail and
        // vice-versa — proves the domain-separation tags are doing their
        // job.
        let k = fixed_key();
        let stored = encrypt_manifest(&k, b"manifest");
        // Try to decrypt the manifest blob as if it were a chunk: should
        // fail because the chunk codepath uses different keys + AAD.
        assert!(matches!(
            crate::chunk::decrypt_chunk(&k, 0, &stored),
            Err(CryptoError::DecryptionFailed)
        ));
    }
}
