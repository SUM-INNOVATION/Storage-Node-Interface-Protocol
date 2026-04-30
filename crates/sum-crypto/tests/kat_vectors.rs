//! Known-answer-test vectors for the upstream primitives `sum-crypto`
//! depends on. These do NOT test our own derivations directly — they
//! re-validate that the underlying RustCrypto crates we link against
//! still produce the canonical RFC outputs. If any of these break, the
//! crate must NOT ship.
//!
//! Sources:
//! * RFC 8439 §2.8.2 — ChaCha20-Poly1305 AEAD test vector
//! * RFC 7748 §6.1 — X25519 test vector
//! * RFC 5869 Appendix A.1 — HKDF-SHA256 test case 1

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

fn hex(s: &str) -> Vec<u8> {
    let s = s.replace([' ', ':', '\n'], "");
    hex::decode(s).expect("valid hex")
}

/// RFC 8439 §2.8.2.
#[test]
fn kat_chacha20poly1305_rfc8439() {
    let plaintext = hex(
        "4c616469657320616e642047656e746c656d656e206f662074686520636c6173\
         73206f66202739393a204966204920636f756c64206f6666657220796f75206f\
         6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73\
         637265656e20776f756c642062652069742e",
    );
    let aad = hex("50515253c0c1c2c3c4c5c6c7");
    let key = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
    let nonce = hex("070000004041424344454647");
    // Expected ciphertext from RFC 8439 §2.8.2:
    let expected_ct_and_tag = hex(
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
         3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
         92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
         3ff4def08e4b7a9de576d26586cec64b6116\
         1ae10b594f09e26a7e902ecbd0600691",
    );

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .unwrap();
    assert_eq!(ct, expected_ct_and_tag);

    // Roundtrip the canonical ct → expected plaintext.
    let pt = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &expected_ct_and_tag,
                aad: &aad,
            },
        )
        .unwrap();
    assert_eq!(pt, plaintext);
}

/// RFC 7748 §6.1. Tests X25519 scalar-multiplication produces the canonical shared secret.
#[test]
fn kat_x25519_rfc7748() {
    let alice_priv: [u8; 32] = hex(
        "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
    )
    .try_into()
    .unwrap();
    let alice_pub: [u8; 32] = hex(
        "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
    )
    .try_into()
    .unwrap();
    let bob_priv: [u8; 32] = hex(
        "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
    )
    .try_into()
    .unwrap();
    let bob_pub: [u8; 32] = hex(
        "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
    )
    .try_into()
    .unwrap();
    let expected_shared: [u8; 32] = hex(
        "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742",
    )
    .try_into()
    .unwrap();

    // Validate public-key derivation on both sides.
    let alice_sec = StaticSecret::from(alice_priv);
    let bob_sec = StaticSecret::from(bob_priv);
    assert_eq!(PublicKey::from(&alice_sec).to_bytes(), alice_pub);
    assert_eq!(PublicKey::from(&bob_sec).to_bytes(), bob_pub);

    // Validate ECDH from both sides.
    let alice_view_pub = PublicKey::from(bob_pub);
    let bob_view_pub = PublicKey::from(alice_pub);
    let alice_shared = alice_sec.diffie_hellman(&alice_view_pub);
    let bob_shared = bob_sec.diffie_hellman(&bob_view_pub);
    assert_eq!(alice_shared.as_bytes(), &expected_shared);
    assert_eq!(bob_shared.as_bytes(), &expected_shared);
}

/// RFC 5869 Appendix A.1 — HKDF-SHA256 test case 1.
#[test]
fn kat_hkdf_sha256_rfc5869_case1() {
    let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let salt = hex("000102030405060708090a0b0c");
    let info = hex("f0f1f2f3f4f5f6f7f8f9");
    let expected_okm = hex(
        "3cb25f25faacd57a90434f64d0362f2a\
         2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
         34007208d5b887185865",
    );

    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut okm = vec![0u8; expected_okm.len()];
    hk.expand(&info, &mut okm).unwrap();
    assert_eq!(okm, expected_okm);
}
