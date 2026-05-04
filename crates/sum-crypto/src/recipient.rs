//! Per-recipient key wrap.
//!
//! The owner generates a single 32-byte file master key `K_file` once
//! per file. To share the file with recipient `r`, the owner produces an
//! 80-byte bundle that lets `r` (and only `r`) recover `K_file`:
//!
//! ```text
//! eph_priv  ← random X25519 scalar (clamped per RFC 7748 by x25519-dalek)
//! eph_pub   = X25519::scalar_mult_base(eph_priv)                                [32 B]
//! shared    = X25519::scalar_mult(eph_priv, r.enc_pub)                          [32 B]
//! kek       = HKDF-SHA256(salt = b"", ikm = shared, info = "snip-recipient-kek-v1")  [32 B]
//! wrap, tag = ChaCha20-Poly1305-Encrypt(plaintext = K_file,
//!                                        key   = kek,
//!                                        nonce = [0; 12],
//!                                        aad   = r.l1_address)                  [32 + 16 B]
//!
//! bundle = eph_pub || wrap || tag                                               [80 B]
//! ```
//!
//! The fixed all-zero nonce is safe: each bundle uses a fresh ephemeral
//! X25519 scalar → fresh `shared` → fresh `kek`. The `kek` encrypts
//! exactly one message (`K_file`), so nonce-reuse is impossible by
//! construction.
//!
//! The recipient's L1 address is bound into the AAD so a bundle cannot
//! be silently rebound to a different address (defense in depth — the
//! chain-side validation already enforces address ↔ bundle pairing).

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use rand_core::{OsRng, RngCore};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

use crate::errors::CryptoError;
use crate::kdf::{hkdf_expand, RECIPIENT_KEK_INFO, X25519_DERIVATION_INFO};

/// Constant-time check that an X25519 ECDH output is non-zero.
///
/// `x25519-dalek` v2 deliberately does NOT validate this — it leaves the
/// decision to the caller, since some protocols want the all-zero output
/// (e.g. ephemeral-ephemeral handshakes that abort cleanly). For SNIP we
/// MUST reject it: an all-zero shared secret means one of the public
/// keys was a low-order point, which would let the corresponding peer
/// (or anyone who planted that key) derive a predictable KEK and learn
/// `K_file`.
///
/// The check is constant-time to avoid leaking timing information about
/// which specific keys triggered the rejection.
fn shared_is_contributory(shared: &SharedSecret) -> bool {
    let zero = [0u8; 32];
    // `ConstantTimeEq::ct_eq` returns `Choice` (0 or 1, lifted to bool
    // without branching). We invert: contributory == not all-zero.
    !bool::from(shared.as_bytes().ct_eq(&zero))
}

/// Total bundle size: `eph_pub (32) || wrap (32) || tag (16)` = 80 bytes.
pub const RECIPIENT_BUNDLE_SIZE: usize = 80;

/// Derive a deterministic X25519 encryption keypair from an Ed25519
/// signing seed. The X25519 private scalar is `HKDF-SHA256(salt = "",
/// ikm = ed25519_seed, info = "snip-x25519-encryption-key-v1")[..32]`,
/// then clamped per RFC 7748 by `x25519_dalek::StaticSecret::from`.
///
/// **Why HKDF, not raw seed reuse:** the same Ed25519 seed already
/// derives the node's signing key, peer-id, and L1 address.
/// Using domain-separated HKDF keeps the encryption-key domain
/// independent — leaking either key (signing or encryption) cannot
/// retroactively recover the other, and a future protocol change to
/// either domain is a one-line `info` rotation rather than a wholesale
/// re-key. The chain plan's locked decision (Phase 4a) requires this
/// exact `info` tag — changing it would break interoperability with
/// previously registered keys.
///
/// Returns `(x25519_private_scalar_bytes, x25519_public_key_bytes)`.
/// The private scalar is the **clamped** scalar (suitable for direct
/// use with `x25519_dalek::StaticSecret`), and the public key is its
/// scalar-base-multiple — the value to register on chain via
/// `RegisterEncryptionKey`.
pub fn x25519_keypair_from_ed25519_seed(
    ed25519_seed: &[u8; 32],
) -> ([u8; 32], [u8; 32]) {
    let derived = hkdf_expand::<32>(&[], ed25519_seed, X25519_DERIVATION_INFO);
    let secret = StaticSecret::from(derived);
    let public = PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

// Bundle layout: [0..32] eph_pub | [32..64] wrap ciphertext | [64..80] tag.
// We only need start/end markers for the eph_pub slice and the bundle
// end; the AEAD ciphertext-and-tag are passed to ChaCha20-Poly1305 as a
// single slice (the AEAD library appends the tag inline on encrypt and
// strips it on decrypt).
const EPH_PUB_OFFSET: usize = 0;
const EPH_PUB_END: usize = 32;
const BUNDLE_END: usize = 80;

const KEK_NONCE: [u8; 12] = [0u8; 12];

/// Wrap `K_file` so that only the owner of the X25519 private key
/// matching `recipient_enc_pub` (with `recipient_l1_address` as AAD) can
/// unwrap it. Uses the OS RNG for the ephemeral scalar.
///
/// # Errors
///
/// Returns [`CryptoError::NonContributoryKey`] when the supplied
/// `recipient_enc_pub` is a low-order point — encrypting `K_file`
/// against it would produce a predictable KEK. Chain-side
/// `RegisterEncryptionKey` should also reject these as a separate layer
/// of defense; this check is a hard belt-and-braces stop for the
/// cryptographic library.
pub fn wrap_for_recipient(
    k_file: &[u8; 32],
    recipient_l1_address: &[u8; 20],
    recipient_enc_pub: &[u8; 32],
) -> Result<[u8; RECIPIENT_BUNDLE_SIZE], CryptoError> {
    // Sample an ephemeral X25519 scalar. `StaticSecret::from` clamps it
    // per RFC 7748 internally.
    let mut eph_seed = [0u8; 32];
    OsRng.fill_bytes(&mut eph_seed);
    let eph_secret = StaticSecret::from(eph_seed);
    let eph_pub = PublicKey::from(&eph_secret);

    // Diffie-Hellman: shared = eph_secret * recipient_pub.
    let recipient_pub = PublicKey::from(*recipient_enc_pub);
    let shared = eph_secret.diffie_hellman(&recipient_pub);

    // Reject all-zero shared secret — see `shared_is_contributory` above.
    if !shared_is_contributory(&shared) {
        return Err(CryptoError::NonContributoryKey);
    }

    // Derive the KEK from the raw shared secret.
    let kek = hkdf_expand::<32>(b"", shared.as_bytes(), RECIPIENT_KEK_INFO);

    // ChaCha20-Poly1305 encrypt K_file with kek, fixed-zero nonce, address AAD.
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&kek));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&KEK_NONCE),
            Payload {
                msg: k_file,
                aad: recipient_l1_address,
            },
        )
        .expect("ChaCha20-Poly1305 wrap of 32-byte K_file cannot fail");
    debug_assert_eq!(ct.len(), 32 + 16);

    let mut bundle = [0u8; RECIPIENT_BUNDLE_SIZE];
    bundle[EPH_PUB_OFFSET..EPH_PUB_END].copy_from_slice(eph_pub.as_bytes());
    bundle[EPH_PUB_END..BUNDLE_END].copy_from_slice(&ct);
    Ok(bundle)
}

/// Unwrap a bundle addressed to `own_l1_address` using the recipient's
/// X25519 private key. Returns the recovered `K_file` or
/// [`CryptoError::DecryptionFailed`] if the bundle was tampered with /
/// not addressed to this account / sealed against a different X25519
/// public key.
pub fn unwrap_for_self(
    bundle: &[u8; RECIPIENT_BUNDLE_SIZE],
    own_enc_priv: &[u8; 32],
    own_l1_address: &[u8; 20],
) -> Result<[u8; 32], CryptoError> {
    let eph_pub_bytes: [u8; 32] = bundle[EPH_PUB_OFFSET..EPH_PUB_END]
        .try_into()
        .expect("static slice into [u8; 32]");
    let wrap_and_tag = &bundle[EPH_PUB_END..BUNDLE_END];

    let own_secret = StaticSecret::from(*own_enc_priv);
    let eph_pub = PublicKey::from(eph_pub_bytes);
    let shared = own_secret.diffie_hellman(&eph_pub);

    // Defense in depth: even on the unwrap side, refuse to derive a KEK
    // from a non-contributory shared secret. A malicious owner could
    // otherwise plant a bundle whose ephemeral public key is a low-
    // order point; the resulting KEK would be derivable by anyone.
    if !shared_is_contributory(&shared) {
        return Err(CryptoError::NonContributoryKey);
    }

    let kek = hkdf_expand::<32>(b"", shared.as_bytes(), RECIPIENT_KEK_INFO);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&kek));
    let pt = cipher
        .decrypt(
            Nonce::from_slice(&KEK_NONCE),
            Payload {
                msg: wrap_and_tag,
                aad: own_l1_address,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)?;

    if pt.len() != 32 {
        return Err(CryptoError::DecryptionFailed);
    }
    let mut k_file = [0u8; 32];
    k_file.copy_from_slice(&pt);
    Ok(k_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x25519_keypair_from_seed(seed: u8) -> ([u8; 32], [u8; 32]) {
        let mut sk = [0u8; 32];
        for (i, b) in sk.iter_mut().enumerate() {
            *b = seed.wrapping_mul((i as u8) | 1);
        }
        let secret = StaticSecret::from(sk);
        let pk: [u8; 32] = PublicKey::from(&secret).to_bytes();
        // We return the *raw* seed bytes (pre-clamping) because that's
        // what the protocol stores. `StaticSecret::from` will clamp on
        // each construction; both the wrap and unwrap codepaths use the
        // same input, so they get the same clamped scalar.
        (sk, pk)
    }

    fn fixed_addr(b: u8) -> [u8; 20] {
        [b; 20]
    }

    fn fixed_kfile() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = ((i as u8) ^ 0xA5).wrapping_mul(3);
        }
        k
    }

    #[test]
    fn bundle_size_constant() {
        assert_eq!(RECIPIENT_BUNDLE_SIZE, 80);
    }

    #[test]
    fn happy_path_roundtrip() {
        let (sk_r, pk_r) = x25519_keypair_from_seed(7);
        let addr_r = fixed_addr(0xAB);
        let k_file = fixed_kfile();

        let bundle = wrap_for_recipient(&k_file, &addr_r, &pk_r).unwrap();
        let recovered = unwrap_for_self(&bundle, &sk_r, &addr_r).unwrap();
        assert_eq!(recovered, k_file);
    }

    #[test]
    fn wrong_recipient_priv_key_rejects() {
        let (_sk_intended, pk_intended) = x25519_keypair_from_seed(7);
        let (sk_other, _) = x25519_keypair_from_seed(8);
        let addr = fixed_addr(0xAB);
        let k_file = fixed_kfile();

        let bundle = wrap_for_recipient(&k_file, &addr, &pk_intended).unwrap();
        // Different private key → different shared → wrong KEK → tag fails.
        assert!(matches!(
            unwrap_for_self(&bundle, &sk_other, &addr),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn wrong_address_aad_rejects() {
        let (sk, pk) = x25519_keypair_from_seed(7);
        let addr_a = fixed_addr(0xAA);
        let addr_b = fixed_addr(0xBB);
        let k_file = fixed_kfile();

        let bundle = wrap_for_recipient(&k_file, &addr_a, &pk).unwrap();
        assert!(matches!(
            unwrap_for_self(&bundle, &sk, &addr_b),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn tampered_bundle_rejected() {
        let (sk, pk) = x25519_keypair_from_seed(7);
        let addr = fixed_addr(0xAB);
        let k_file = fixed_kfile();

        // Flip a bit in the ephemeral public key.
        let mut bundle = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        bundle[0] ^= 0x01;
        // Note: not all single-bit flips of eph_pub yield a valid
        // contributory point — some land on a low-order one and trip
        // the contributory-key check before the AEAD has a chance to
        // notice. Either rejection is acceptable here (the bundle
        // still ends up unauthorized), so we accept either error.
        assert!(matches!(
            unwrap_for_self(&bundle, &sk, &addr),
            Err(CryptoError::DecryptionFailed) | Err(CryptoError::NonContributoryKey)
        ));

        // Flip a bit in the wrap ciphertext.
        let mut bundle = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        bundle[40] ^= 0x80;
        assert!(matches!(
            unwrap_for_self(&bundle, &sk, &addr),
            Err(CryptoError::DecryptionFailed)
        ));

        // Flip a bit in the tag (last 16 bytes).
        let mut bundle = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        let last = bundle.len() - 1;
        bundle[last] ^= 0x01;
        assert!(matches!(
            unwrap_for_self(&bundle, &sk, &addr),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn each_wrap_uses_fresh_ephemeral_key() {
        // Two wraps for the same recipient, same K_file → distinct
        // bundles (because the ephemeral X25519 scalar is freshly
        // sampled). This is critical for the fixed-zero-nonce safety
        // argument in the doc comment.
        let (_, pk) = x25519_keypair_from_seed(7);
        let addr = fixed_addr(0xAB);
        let k_file = fixed_kfile();
        let b1 = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        let b2 = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        assert_ne!(b1, b2, "ephemeral scalar must differ across wraps");
    }

    /// `wrap_for_recipient` MUST refuse to encrypt against a low-order
    /// X25519 public key — encrypting against one would seal `K_file`
    /// under a predictable KEK derivable by any holder of the matching
    /// low-order point.
    ///
    /// Per RFC 7748 §6.1 (and a long line of crypto-library audits) the
    /// canonical low-order points are well known. We use the all-zero
    /// public key as the simplest representative — any X25519 scalar
    /// times the all-zero point yields the all-zero shared secret.
    #[test]
    fn wrap_rejects_low_order_recipient_pubkey() {
        let zero_pub = [0u8; 32];
        let addr = fixed_addr(0xCD);
        let k_file = fixed_kfile();
        let res = wrap_for_recipient(&k_file, &addr, &zero_pub);
        assert!(
            matches!(res, Err(CryptoError::NonContributoryKey)),
            "wrap must reject low-order pubkey, got {res:?}"
        );
    }

    /// Symmetric defense on the unwrap side: a bundle whose ephemeral
    /// public key is the all-zero point must be rejected before any
    /// KEK material is derived, regardless of the AEAD outcome.
    #[test]
    fn unwrap_rejects_low_order_ephemeral_pubkey() {
        let (sk, _pk) = x25519_keypair_from_seed(7);
        let addr = fixed_addr(0xCD);
        // Hand-construct a bundle: eph_pub = 0, junk wrap+tag.
        let mut bundle = [0u8; RECIPIENT_BUNDLE_SIZE];
        bundle[EPH_PUB_END..BUNDLE_END].copy_from_slice(&[0xAA; 48]);
        let res = unwrap_for_self(&bundle, &sk, &addr);
        assert!(
            matches!(res, Err(CryptoError::NonContributoryKey)),
            "unwrap must reject low-order eph_pub, got {res:?}"
        );
    }

    // ── Phase 4a: x25519_keypair_from_ed25519_seed ─────────────────────

    /// Same Ed25519 seed always yields the same X25519 keypair. Locks
    /// the HKDF derivation contract — if anyone changes the `info` tag
    /// or the salt, every existing registered key on chain becomes
    /// orphaned (the X25519 private key for an account would no longer
    /// match its registered public key).
    #[test]
    fn x25519_derivation_is_deterministic() {
        let seed = [0x42u8; 32];
        let (sk_a, pk_a) = x25519_keypair_from_ed25519_seed(&seed);
        let (sk_b, pk_b) = x25519_keypair_from_ed25519_seed(&seed);
        assert_eq!(sk_a, sk_b);
        assert_eq!(pk_a, pk_b);
    }

    /// Different Ed25519 seeds yield different X25519 keypairs.
    #[test]
    fn x25519_derivation_changes_with_seed() {
        let (_, pk_1) = x25519_keypair_from_ed25519_seed(&[0x01; 32]);
        let (_, pk_2) = x25519_keypair_from_ed25519_seed(&[0x02; 32]);
        assert_ne!(pk_1, pk_2);
    }

    /// Round-trip: derive a keypair from a seed, wrap K_file for the
    /// derived public key, unwrap with the derived private key. Proves
    /// the derived bytes are a valid X25519 keypair end-to-end (not
    /// just plausible-looking 32-byte values).
    #[test]
    fn x25519_derivation_round_trips_through_wrap_unwrap() {
        let seed = [0x99u8; 32];
        let (sk, pk) = x25519_keypair_from_ed25519_seed(&seed);
        let addr = fixed_addr(0xAB);
        let k_file = fixed_kfile();
        let bundle = wrap_for_recipient(&k_file, &addr, &pk).unwrap();
        let recovered = unwrap_for_self(&bundle, &sk, &addr).unwrap();
        assert_eq!(recovered, k_file);
    }
}
