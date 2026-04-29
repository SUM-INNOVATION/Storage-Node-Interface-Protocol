//! HKDF-SHA256 helpers used by the chunk and manifest layers.
//!
//! All derivations use explicit `salt` and `info` parameters to keep the
//! crypto-spec close to the call site. Domain separation is enforced via
//! the `info` parameter — the strings are part of the V1 wire format and
//! must not be reused.

use hkdf::Hkdf;
use sha2::Sha256;

/// Domain-separation tag for per-chunk content keys.
pub(crate) const CHUNK_KEY_INFO: &[u8] = b"snip-chunk-key-v1";
/// Domain-separation tag for per-chunk AEAD nonces.
pub(crate) const CHUNK_NONCE_INFO: &[u8] = b"snip-chunk-nonce-v1";
/// Domain-separation tag for the manifest content key.
pub(crate) const MANIFEST_KEY_INFO: &[u8] = b"snip-manifest-key-v1";
/// Domain-separation tag for the per-recipient KEK derived from X25519 ECDH.
pub(crate) const RECIPIENT_KEK_INFO: &[u8] = b"snip-recipient-kek-v1";

/// Derive `OUT_LEN` bytes via HKDF-SHA256.
///
/// `salt` may be empty (e.g. for the recipient KEK where the ECDH shared
/// secret already provides freshness). `info` MUST be one of the
/// constants in this module — using a fresh string would silently make
/// derived keys incompatible with the rest of the protocol.
pub(crate) fn hkdf_expand<const OUT_LEN: usize>(
    salt: &[u8],
    ikm: &[u8],
    info: &[u8],
) -> [u8; OUT_LEN] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8; OUT_LEN];
    // HKDF-SHA256 supports up to 255 * 32 = 8160 output bytes, far more
    // than any caller in this crate needs. The `expand` call only fails
    // if `OUT_LEN` exceeds that, which is a programming error here.
    hk.expand(info, &mut out)
        .expect("HKDF-SHA256 cannot fail for OUT_LEN <= 8160");
    out
}
