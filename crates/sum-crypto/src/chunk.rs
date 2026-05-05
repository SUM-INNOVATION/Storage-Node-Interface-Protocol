//! Per-chunk AEAD encryption.
//!
//! For chunk index `i` ∈ `[0, chunk_count)`:
//!
//! ```text
//! k_i  = HKDF-SHA256(salt = i.to_be_bytes(),  ikm = K_file, info = "snip-chunk-key-v1")
//! n_i  = HKDF-SHA256(salt = i.to_be_bytes(),  ikm = K_file, info = "snip-chunk-nonce-v1")[..12]
//! ct_i = ChaCha20-Poly1305-Encrypt(plaintext = pt_i,
//!                                   key = k_i,
//!                                   nonce = n_i,
//!                                   aad = i.to_be_bytes())
//! on_disk_i = ct_i || tag_i      // ChaCha20-Poly1305 returns ct||tag inline
//! ```
//!
//! The AAD is only `chunk_index_be(4)` — there is no `merkle_root` in
//! the AAD. Cross-file substitution is prevented by `K_file` being
//! unique per file (different K_file → different per-chunk keys → AEAD
//! tag check fails on swap). Within-file substitution is prevented by
//! the AAD binding the chunk to its position.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};

use crate::errors::CryptoError;
use crate::kdf::{CHUNK_KEY_INFO, CHUNK_NONCE_INFO, hkdf_expand};

/// Encrypt a single plaintext chunk. Returns `ciphertext || tag` ready
/// to be hashed (blake3) and written to disk.
///
/// `K_file` is the 32-byte file master key — the same value is reused
/// for every chunk in this file.
pub fn encrypt_chunk(k_file: &[u8; 32], chunk_index: u32, plaintext: &[u8]) -> Vec<u8> {
    let salt = chunk_index.to_be_bytes();
    let key_bytes = hkdf_expand::<32>(&salt, k_file, CHUNK_KEY_INFO);
    let nonce_bytes = hkdf_expand::<12>(&salt, k_file, CHUNK_NONCE_INFO);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &salt,
            },
        )
        // ChaCha20-Poly1305 encryption can only fail when the message
        // exceeds 2^32-1 blocks (~256 GiB). Chunks are 1 MiB.
        .expect("ChaCha20-Poly1305 encrypt cannot fail for chunk-sized inputs")
}

/// Decrypt a single chunk. Returns the plaintext or
/// [`CryptoError::DecryptionFailed`] on tag mismatch.
pub fn decrypt_chunk(
    k_file: &[u8; 32],
    chunk_index: u32,
    on_disk: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let salt = chunk_index.to_be_bytes();
    let key_bytes = hkdf_expand::<32>(&salt, k_file, CHUNK_KEY_INFO);
    let nonce_bytes = hkdf_expand::<12>(&salt, k_file, CHUNK_NONCE_INFO);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
    cipher
        .decrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: on_disk,
                aad: &salt,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TAG_SIZE;

    fn fixed_key() -> [u8; 32] {
        // Arbitrary deterministic key for tests. NOT a security claim —
        // these tests don't verify privacy, only correctness.
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        k
    }

    #[test]
    fn roundtrip_one_chunk() {
        let k = fixed_key();
        let pt = b"hello SNIP";
        let ct = encrypt_chunk(&k, 0, pt);
        // Ciphertext is plaintext-len + 16-byte tag.
        assert_eq!(ct.len(), pt.len() + TAG_SIZE);
        let recovered = decrypt_chunk(&k, 0, &ct).unwrap();
        assert_eq!(recovered, pt);
    }

    #[test]
    fn ciphertext_grows_by_tag_size() {
        let k = fixed_key();
        for &n in &[0usize, 1, 17, 1024, 1024 * 1024] {
            let pt = vec![0xAB; n];
            let ct = encrypt_chunk(&k, 42, &pt);
            assert_eq!(ct.len(), n + TAG_SIZE, "n={n}");
        }
    }

    #[test]
    fn distinct_chunk_indices_yield_distinct_ciphertexts() {
        let k = fixed_key();
        let pt = b"identical plaintext";
        let ct0 = encrypt_chunk(&k, 0, pt);
        let ct1 = encrypt_chunk(&k, 1, pt);
        assert_ne!(ct0, ct1);
    }

    #[test]
    fn cross_index_decrypt_fails() {
        // A chunk encrypted as index 0 must not decrypt as index 1.
        let k = fixed_key();
        let pt = b"chunk zero";
        let ct = encrypt_chunk(&k, 0, pt);
        assert!(matches!(
            decrypt_chunk(&k, 1, &ct),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn cross_file_decrypt_fails() {
        // A chunk encrypted under K_file_A must not decrypt under K_file_B.
        let k_a = fixed_key();
        let mut k_b = k_a;
        k_b[0] ^= 1;
        let pt = b"some bytes";
        let ct = encrypt_chunk(&k_a, 7, pt);
        assert!(matches!(
            decrypt_chunk(&k_b, 7, &ct),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let k = fixed_key();
        let pt = vec![0xCC; 64];
        let mut ct = encrypt_chunk(&k, 9, &pt);
        // Flip a single bit in the ciphertext body.
        ct[0] ^= 0x80;
        assert!(matches!(
            decrypt_chunk(&k, 9, &ct),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let k = fixed_key();
        let pt = vec![0xCC; 64];
        let mut ct = encrypt_chunk(&k, 9, &pt);
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(matches!(
            decrypt_chunk(&k, 9, &ct),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn large_chunk_roundtrips() {
        // 1 MiB — the protocol's plaintext chunk size.
        let k = fixed_key();
        let pt = vec![0x5A; 1 << 20];
        let ct = encrypt_chunk(&k, 17, &pt);
        let recovered = decrypt_chunk(&k, 17, &ct).unwrap();
        assert_eq!(recovered, pt);
    }
}
