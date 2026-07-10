# Threat model

Where SNIP puts a security boundary, what it defends against, and
what it does not. This document is the *why* behind the shipped
design. The pinning guards that keep each defence honest live in
[`privacy-audit.md`](privacy-audit.md); this document explains the
reasoning above those guards.

The reader should also see [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md)
for chain-side enforcement of storage integrity, which is a
separate mechanism from the confidentiality boundary discussed
here.

## Actors and what they can do

- **File owner (Alice).** Registers a file on chain, pushes chunks
  to the assigned archives, controls the access list.
- **File reader (Bob).** On a Private file's access list. Holds
  their own Ed25519 seed; can derive their X25519 secret; can
  decrypt what the chain permits them to see.
- **Third party (Carol).** Not on any access list.
- **Malicious archive operator (Eve).** Runs a modified `sum-node
  listen`. Can read the archive's on-disk chunk directory
  directly; can serve chunks to anyone regardless of ACL. Can lie
  about assignment coverage; can drop chunks; can attempt to skip
  Merkle-path verification on push.
- **Passive network observer.** Watches libp2p and RPC traffic.
- **Chain-level adversary.** Compromises validator quorum. (Out
  of scope; SNIP inherits SUM Chain's threat model here.)

## Confidentiality boundary

**The security boundary is the encryption key, not the archive.**

For Public files SNIP does not attempt confidentiality — Public
files are world-readable by design. The chunk bytes on disk are
plaintext; anyone who can reach an archive can request them; the
archive serves anyone who passes the (empty) ACL.

For Private files the confidentiality boundary is the per-file
symmetric key `K_file`:

1. On ingest, the client generates a fresh `K_file` (ChaCha20-Poly1305
   key). Every chunk and the manifest are encrypted under `K_file`
   before leaving the client.
2. `K_file` is wrapped for each recipient using X25519 hybrid
   encryption: the client encrypts `K_file` for the recipient's
   registered X25519 pubkey and stores the resulting 80-byte
   ciphertext (`encrypted_key_bundle`) on chain as an `AccessEntryV2`.
3. To read the file, a recipient reads their own bundle from
   chain, unwraps it with their X25519 secret (derived
   deterministically from their Ed25519 seed), and decrypts the
   ciphertext chunks.
4. Archives hold **ciphertext only**. A malicious archive with
   `cat ~/.sumnode/store/*.chunk` sees random bytes.

This is why the shipped design was not the "Deterministic + ACL"
approach that the original planning document
([`../archive/SECURITY-ANALYSIS.md`](../archive/SECURITY-ANALYSIS.md))
described as the interim option: ACL-only enforcement makes the
archive operator's honesty the security boundary. Encryption
makes the key the boundary instead — anyone with only ciphertext,
including Eve, sees nothing useful.

## What each actor can do under the shipped design

| Actor | Public file | Private file |
|---|---|---|
| Alice (owner) | Reads + writes | Reads + writes |
| Bob (on ACL) | Reads | Reads |
| Carol (off ACL) | Reads | Denied at archive ACL; even if bytes exfiltrate, they are ciphertext without the key |
| Eve (malicious archive) | Reads plaintext (as designed for Public) | Reads only ciphertext; cannot decrypt without a wrapped-bundle key it does not have |
| Passive observer | Sees chunk sizes and archive addresses | Same visibility; content is ciphertext |

Note that "denied at archive ACL" is a soft guarantee: a modified
`sum-node` binary can be programmed to serve regardless of ACL.
Under the encryption boundary, that modified binary hands out
ciphertext to whoever asks — the encryption still protects the
content. The chain-side ACL then becomes a **bandwidth
optimization**, not the security boundary.

## Storage integrity (a separate boundary)

Confidentiality is one boundary; **storage integrity** is another.
SNIP defends the "the archive actually holds what it claims" boundary
via:

- Chain-side deterministic assignment. Every participant computes
  the same per-chunk archive set. Uploading a chunk to the "wrong"
  archive is a wire-level error rejected by
  `PushValidator::validate_push`.
- Chain-side coverage attestation. Archives submit
  `AcceptAssignmentV2` bitmaps that the chain OR-merges. Files
  activate only when a chain-computed coverage predicate passes.
- Chain-side Proof of Retrievability. Randomly-timed challenges
  targeting specific `(archive, file, chunk_index)` tuples, with
  a 50-block deadline and a 5% slash. Targeting rules depend on
  the `assignment_targeting` gate — see
  [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md).

None of these are cryptographic guarantees against a chain-level
adversary — they inherit the SUM Chain's Byzantine-fault
assumptions. Within those assumptions, an archive that cheats
loses stake until it is ejected from the network.

## What SNIP does NOT defend against

- **Forward secrecy on revoke.** `revoke` removes the chain-side
  access entry but does not rotate `K_file`. A revoked recipient
  who cached the ciphertext + their old bundle can still decrypt
  what they had access to. Row 14 of [`privacy-audit.md`](privacy-audit.md)
  documents this; the roadmap tracks rotation as a planned item.
- **Traffic analysis.** The observer sees chunk-size histograms
  and archive addresses. Deduplication is not implemented, so
  identical plaintext ingested under different keys produces
  different ciphertext and different merkle roots; identical
  ciphertext is not currently cross-referenced across files.
- **Chain-level attacks.** A compromised validator quorum could
  in principle rewrite access lists or falsify PoR results. SNIP
  inherits the SUM Chain's assumptions here without additional
  defence.
- **Key loss.** Lose the Ed25519 seed → lose every Private file
  that was ingested or received under it. There is no
  protocol-level recovery. See
  [`../client/key-management.md`](../client/key-management.md).
- **Side channels.** Timing / power / cache side channels against
  ChaCha20-Poly1305 or X25519 are not analyzed in this repo.

## Cross-references

- Pinning guardrails per row: [`privacy-audit.md`](privacy-audit.md).
- Chain-side retention enforcement: [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md).
- Historical design analysis (superseded): [`../archive/SECURITY-ANALYSIS.md`](../archive/SECURITY-ANALYSIS.md).
- Operator-side privacy posture: [`../operator/runbook.md`](../operator/runbook.md).
