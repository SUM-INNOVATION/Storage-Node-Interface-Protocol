# sum-crypto

The encryption crate. It provides everything a private file needs: per-chunk and
per-manifest AEAD, a per-recipient key-wrap scheme, and deterministic derivation
of a node's X25519 encryption keypair from its Ed25519 seed. It is a leaf crate
with no internal dependencies, so its contract is a stable anchor. Source:
[`crates/sum-crypto`](../../crates/sum-crypto).

Everything here is applied only to **private** files. Public files are stored and
served in the clear.

## Primitives

| Role | Primitive | Notes |
|------|-----------|-------|
| AEAD cipher | ChaCha20-Poly1305 (RFC 8439) | 32-byte key, 12-byte nonce, 16-byte tag (`TAG_SIZE = 16`) |
| KDF | HKDF-SHA256 (RFC 5869) | one generic `hkdf_expand` used by all derivations |
| Key exchange | X25519 (RFC 7748) | 32-byte scalar and public key |
| Hash | SHA-256 | inside HKDF |

The choice of a single AEAD, a single KDF, and a single curve keeps the audit
surface small. The `sum-crypto` public surface, re-exported from `lib.rs`, is:
`encrypt_chunk` / `decrypt_chunk`, `encrypt_manifest` / `decrypt_manifest`,
`wrap_for_recipient` / `unwrap_for_self`, `x25519_keypair_from_ed25519_seed`,
`RECIPIENT_BUNDLE_SIZE`, and `CryptoError`.

## Domain separation

Every derivation is domain-separated by a versioned `info` string, so a key
derived for one purpose can never collide with one derived for another. The exact
strings (from [`kdf.rs`](../../crates/sum-crypto/src/kdf.rs)):

| Constant | Value | Derives |
|----------|-------|---------|
| `CHUNK_KEY_INFO` | `snip-chunk-key-v1` | per-chunk encryption key |
| `CHUNK_NONCE_INFO` | `snip-chunk-nonce-v1` | per-chunk nonce |
| `MANIFEST_KEY_INFO` | `snip-manifest-key-v1` | manifest encryption key |
| `RECIPIENT_KEK_INFO` | `snip-recipient-kek-v1` | per-recipient key-encryption key |
| `X25519_DERIVATION_INFO` | `snip-x25519-encryption-key-v1` | the account's X25519 keypair |

The `-v1` suffix is the versioning hook: a future scheme change ships as `-v2`
without invalidating existing derivations.

## K_file: the per-file master key

Each private file has one 32-byte master key, `K_file` (`K_FILE_SIZE = 32`),
generated randomly at ingest. It is never stored on chain and never written to
disk in the clear. Everything else about the file's encryption derives from it:

- **Chunk key** for index `i`: `HKDF(salt = i.to_be_bytes(), ikm = K_file, info = "snip-chunk-key-v1")`.
- **Chunk nonce** for index `i`: the first 12 bytes of
  `HKDF(salt = i.to_be_bytes(), ikm = K_file, info = "snip-chunk-nonce-v1")`.
- **Manifest key**: `HKDF(ikm = K_file, info = "snip-manifest-key-v1")`.

Because the key and nonce are both derived per chunk index, no two chunks share
an encryption context, and the derivation is deterministic, so a node can
re-derive them from `K_file` alone.

## Chunk encryption

`encrypt_chunk(k_file, chunk_index, plaintext) -> Vec<u8>` derives the chunk key
and nonce as above, then AEAD-encrypts with the 4-byte big-endian chunk index as
associated data (AAD). The on-disk form is `ciphertext || tag`, exactly 16 bytes
larger than the plaintext. That 16-byte overhead per chunk is why a private
file's `stored_size_bytes` on chain is `plaintext + 16 × chunk_count`.

`decrypt_chunk` reverses it and, because the chunk index is bound as AAD, rejects
three distinct attacks with one tag check: tampering (ciphertext altered),
cross-index substitution (a chunk moved to a different position), and cross-file
substitution (prevented additionally by the unique `K_file` per file). A failure
surfaces as `CryptoError::DecryptionFailed`, which is deliberately opaque: it
does not distinguish "wrong key" from "tampered" from "wrong AAD," so it leaks
nothing to an attacker probing the boundary.

## Manifest encryption

`encrypt_manifest` / `decrypt_manifest` encrypt the serialized `DataManifest`
under the manifest key with a fixed literal AAD (`snip-manifest-v1`). The nonce
is all-zero, which is safe because the manifest key is unique per file (derived
from that file's `K_file`) and encrypts exactly one message.

## Per-recipient key wrap

The heart of sharing. `wrap_for_recipient` lets the owner grant a recipient
access to `K_file` without ever revealing it to the chain. Given the recipient's
registered X25519 public key and their 20-byte L1 address, it:

1. Samples a fresh ephemeral X25519 scalar and computes its public key.
2. Does ECDH between the ephemeral scalar and the recipient's public key to get a
   shared secret, and rejects a low-order (non-contributory) recipient key with
   `CryptoError::NonContributoryKey` (a constant-time check, so the KEK can never
   be predictable).
3. Derives a key-encryption key: `HKDF(ikm = shared, info = "snip-recipient-kek-v1")`.
4. AEAD-encrypts `K_file` under the KEK, with the recipient's L1 address as AAD
   (binding the bundle to that address so it cannot be silently reused for
   another), and an all-zero nonce (safe: each wrap has a fresh ephemeral scalar,
   hence a fresh KEK encrypting exactly one message).

The result is the **80-byte bundle** (`RECIPIENT_BUNDLE_SIZE = 80`):

```
[ ephemeral X25519 public key : 32 ][ wrapped K_file ciphertext : 32 ][ Poly1305 tag : 16 ]
```

This is the `encrypted_key_bundle` stored per access-list entry on chain (as a
0x-prefixed 160-hex-char string). `unwrap_for_self` runs the mirror operation
with the recipient's X25519 private key to recover `K_file`.

## X25519 keypair from the Ed25519 seed

A node has one secret: its Ed25519 seed (its wallet and identity). Its encryption
keypair is derived from that same seed by
`x25519_keypair_from_ed25519_seed(ed25519_seed) -> (private, public)`, using
HKDF with `info = "snip-x25519-encryption-key-v1"`. This is why
`register-encryption-key` needs only the key file: it derives the X25519 keypair
deterministically and publishes the public half. The same domain string is
mirrored on the chain client side (the RPC method
`account_getEncryptionPublicKey` returns exactly this key).

## Errors

`CryptoError` has three variants: `InvalidLength { expected, got }` for malformed
inputs, `DecryptionFailed` (opaque, for any AEAD failure), and
`NonContributoryKey` for a low-order X25519 point.

## Known-answer tests

[`tests/kat_vectors.rs`](../../crates/sum-crypto/tests/kat_vectors.rs) pins the
three underlying primitives to their RFC test vectors: ChaCha20-Poly1305 against
RFC 8439 §2.8.2, X25519 against RFC 7748 §6.1, and HKDF-SHA256 against RFC 5869
Appendix A.1. These guard against a silent regression in an upstream RustCrypto
dependency. They validate the primitives, not `sum-crypto`'s own derivations,
which are covered by the unit tests in each module.

## See also

- [`PRIVATE-FILES.md`](../guides/PRIVATE-FILES.md): the operator-facing how-to
- [`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md): the threat model these primitives serve
- [`OVERVIEW.md`](OVERVIEW.md): where this crate sits in the workspace
