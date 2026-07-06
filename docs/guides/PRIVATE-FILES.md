# Private files

How to publish an encrypted file, share it with specific recipients, and manage
access over time. Private files are end-to-end encrypted: the chain never sees
the file contents or the file encryption key, only per-recipient wrapped key
bundles and the access list.

For the cryptographic design behind this, see
[`architecture/SUM-CRYPTO.md`](../architecture/SUM-CRYPTO.md). This guide is the
operational how-to.

## The model in one paragraph

Each private file gets a fresh 32-byte file key, `K_file`, generated at ingest.
Every chunk and the manifest are encrypted under keys derived from `K_file`. For
each recipient, `K_file` is wrapped (encrypted) to that recipient's registered
X25519 public key, producing an 80-byte bundle stored on chain in the file's
access list. A recipient downloads by unwrapping `K_file` from their own bundle
using their private key, then decrypting the chunks. The chain stores the
bundles and the access list; it never holds `K_file` itself.

## Prerequisite: recipients must register an encryption key

Before anyone can share a private file with an account, that account must
register its X25519 encryption public key on chain:

```bash
sum-node --key-file recipient.hex register-encryption-key
```

This derives the X25519 keypair deterministically from the account's Ed25519
seed (HKDF domain `snip-x25519-encryption-key-v1`) and publishes only the public
key. It is idempotent. An account without a registered key cannot receive
private shares, and any attempt to share with them aborts before touching chain
state.

## Publish a private file

```bash
sum-node --client --key-file alice.hex \
    ingest-v2 ./report.pdf \
    --visibility private \
    --recipient <bob_addr>:6000000 \
    --recipient <carol_addr>
```

- `--visibility private` triggers key generation and encryption of every chunk
  and the manifest.
- Each `--recipient` is a base58 L1 address, optionally suffixed with
  `:<expires_at_height>`. Bob's access expires at block 6,000,000; Carol's has
  no expiry.
- The owner (Alice) is added automatically, so she can always recover the file.
- Each recipient's X25519 public key is fetched from chain. If any recipient has
  no registered key, ingest aborts **before** creating any chain state, so you
  do not pay for a half-published file.

The stored size on chain is larger than the plaintext: each chunk carries a
16-byte authentication tag, so `stored_size_bytes = plaintext + 16 × chunk_count`.

## Download a private file

Identical to a public download; the client detects visibility from the chain row
and takes the private path automatically:

```bash
sum-node --client --key-file bob.hex \
    download 34a749...1b66 --output ./report.pdf
```

The client fetches Bob's own bundle from the access list, unwraps `K_file` with
Bob's X25519 private key, decrypts each chunk, and verifies. Bob must be a
current, unexpired entry; a revoked or expired entry is denied at the serving
archive and cannot unwrap.

## Share with a new recipient later

Owner-only. Adds an access entry without re-ingesting the file:

```bash
sum-node --client --key-file alice.hex \
    share 34a749...1b66 --recipient <dave_addr>:6500000
```

Alice recovers `K_file` locally from her own bundle, wraps it for Dave's
registered X25519 key, and submits `AddAccessV2`. `K_file` never leaves Alice's
machine in plaintext. Dave must have registered an encryption key first.

Recipient spec forms:
- `<addr>` grant with no expiry
- `<addr>:<height>` grant expiring at that block height
- `<addr>:none` grant with an explicit no-expiry

## Change a recipient's expiry

Owner-only. Adjusts only the expiry, preserving the recipient's existing wrapped
key bundle byte-for-byte:

```bash
sum-node --client --key-file alice.hex \
    update-access 34a749...1b66 --recipient <bob_addr>:7000000   # extend
sum-node --client --key-file alice.hex \
    update-access 34a749...1b66 --recipient <bob_addr>:none       # clear expiry
```

The expiry directive is mandatory: a bare `<addr>` is rejected so your intent is
never ambiguous.

## Revoke a recipient

Owner-only. Removes the recipient's access entry:

```bash
sum-node --client --key-file alice.hex \
    revoke 34a749...1b66 --recipient <bob_addr>
```

After the `RemoveAccessV2` finalizes, serving archives deny Bob on his next
pull.

### Important: revocation is not forward-secret

Revocation removes the chain access entry but does **not** rotate `K_file`. A
revoked recipient who already cached the ciphertext and their wrapped bundle
before revocation can still decrypt the content they had access to. There is no
way to claw back what someone already downloaded. If you need forward secrecy
(the revoked party must lose access to content they have not yet fetched),
revoke and then re-ingest the file under a fresh `K_file`, which produces a new
merkle root. This is documented as a known limitation in
[`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md) (threat 14) and
[`SECURITY.md`](../../SECURITY.md).

## See also

- [`SUM-CRYPTO.md`](../architecture/SUM-CRYPTO.md): the encryption and key-wrap design
- [`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md): the full threat model
- [`CLI.md`](../reference/CLI.md): every flag and subcommand
