# Security Analysis: Storage Access Control & Encryption

## Context

The SUM Storage Node Protocol stores file chunks on untrusted third-party nodes. This document analyzes three approaches to controlling who can access stored data, their security properties, and the recommended path forward.

---

## The Threat Model

**Actors:**
- **Alice** — uploads a private file (e.g., medical records) with `access_list: [Bob_address]`
- **Bob** — authorized to download the file
- **Carol** — unauthorized, wants to read the file
- **Eve** — operates storage node N1, which holds chunks of Alice's file

**What we need to protect:**
- File content confidentiality (Carol and Eve should not be able to read Alice's data)
- Access control enforcement (only Bob should be able to retrieve the file)
- Metadata privacy (minimize what observers learn about stored files)

---

## Approach 1: Current Implementation (Deterministic Assignment + Off-Chain ACL)

### How it works

`compute_chunk_assignment()` deterministically maps chunks to nodes using blake3. Nodes must store what the math says. Before serving a chunk, the node queries the L1 for the file's `access_list` and checks whether the requester's L1 address is in the list. If not, the request is denied.

### Pros

- Zero coordination — every participant computes the same assignment independently
- Guaranteed R=3 redundancy from the moment of upload
- No on-chain storage of assignment data (pure computation)
- Self-healing via MarketSync when nodes join/leave
- ACL is simple to understand and implement

### Security Issues

1. **Node operators have plaintext data on their filesystem.** Eve can run `cat ~/.sumnode/store/*.chunk` and read Alice's medical records directly. The ACL controls the P2P serving path, not filesystem access.

2. **ACL is enforced by the node's own binary.** Eve modifies her `sum-node` build to skip the ACL check — one line removed from `serve.rs`. The L1 has no way to detect this. Eve serves Alice's data to Carol without consequence.

3. **Gossipsub announcements leak metadata.** All mesh participants see `ChunkAnnouncement` messages containing merkle_root, chunk_index, CID, and size. An observer learns which files exist, how large they are, and which nodes hold which chunks — without requesting any data.

4. **ACL revocation is retroactive-only.** If Bob downloaded the file before being removed from the access_list, he keeps the data. Revocation prevents future downloads but not past ones.

5. **Assignment is publicly computable.** Anyone with on-chain state can determine exactly which 3 nodes hold any chunk of any file. This enables targeted attacks against specific files.

### Attacker Capabilities

| Attacker | Capability |
|----------|-----------|
| Malicious node operator (Eve) | Full read access to all stored data. Can serve to anyone. Can exfiltrate silently. |
| Network observer | Learns full storage topology from gossipsub. Knows file sizes and chunk counts. |
| Targeted attacker | Computes which 3 nodes to compromise for any specific file. |

---

## Approach 2: Opt-In Model (Nodes Choose What to Store)

### How it works

Nodes submit `RegisterAsProvider` transactions declaring which files they'll store. The L1 records `storage_providers: Vec<Address>` per file. Challenges only target providers for that file. Nodes earn rewards only from files they opted into.

### Pros

- Nodes control their economics — store profitable files, skip unprofitable ones
- No punishment for not storing files you never committed to
- Natural market dynamics — well-funded files attract more providers

### Security Issues

All of Approach 1's issues, plus:

1. **Breaks guaranteed redundancy.** A new file may have 0 providers until nodes decide to opt in. Underfunded files may never attract R providers.

2. **Race condition on upload.** Alice uploads and waits for providers to register. Until R nodes opt in, the file has insufficient redundancy. If Alice disconnects early, the file may sit with 0-1 providers indefinitely.

3. **On-chain bloat.** Every opt-in is a transaction. With 1000 files and 10 nodes, potentially 10,000 `RegisterAsProvider` transactions.

4. **Sybil gaming.** One operator runs 10 nodes, opts all into high-reward files, concentrates earnings. Under deterministic assignment, the math distributes fairly.

5. **Provider churn.** When a provider leaves, no automatic replacement. Under deterministic assignment, MarketSync automatically redistributes.

6. **Challenge gaming.** Opt in, fetch chunk only when challenged, pass proof, delete between challenges. Node passes every PoR check but provides terrible retrieval availability.

7. **Selective censorship.** Nodes can collectively refuse to opt into specific files, effectively censoring them from the network.

### Attacker Capabilities

All of Approach 1, plus:
- Selectively opt into target files to gain legitimate access to chunk data
- Manipulate file availability by refusing to provide storage for specific files
- Concentrate rewards via Sybil registration

---

## Approach 3: Client-Side Encryption (Encrypt Before Chunking)

### How it works

Alice encrypts `file.pdf` with a symmetric key (XChaCha20-Poly1305) before chunking. Only Alice and authorized recipients hold the key. Nodes store, serve, and prove ciphertext. The key is shared out-of-band (not on-chain).

```
key = random 256-bit XChaCha20-Poly1305 key
encrypted_file = encrypt(key, file.pdf)
chunks = chunk(encrypted_file)     ← nodes store ciphertext, never plaintext
merkle_root = merkle_tree(chunks)  ← computed over ciphertext
```

### Pros

- **Node operators see random noise** — plaintext never exists on untrusted infrastructure
- **Modified binary is harmless** — Eve serves ciphertext to Carol, who can't decrypt it without the key
- **Gossipsub metadata reveals nothing about content** — only encrypted chunk sizes (uniform 1 MB)
- **ACL becomes a bandwidth optimization, not the security boundary** — the key IS the access control
- **Works with any assignment model** — encryption is orthogonal to how chunks are distributed
- **Revocation is meaningful** — don't share the key with Carol, Carol never decrypts, period
- **Storage topology knowledge is useless** — knowing which nodes hold chunks of encrypted data reveals nothing about the content

### Security Issues

1. **Key management is Alice's responsibility.** Lose the key, lose the file forever. No protocol-level recovery mechanism.

2. **Key distribution is out-of-band.** The protocol doesn't handle "how does Bob get the key?" Alice must use a secure channel (encrypted messaging, in-person, QR code, etc.).

3. **Re-encryption is expensive.** If Alice wants to add Carol after upload, she either shares the same key (Carol has permanent access) or re-encrypts and re-uploads the entire file with a new key.

4. **No deduplication.** Two uploads of the same file with different keys produce different merkle_roots and different ciphertext. The network stores duplicate data without knowing it.

5. **Key compromise is catastrophic.** If the symmetric key leaks, all security is lost. Anyone with the key + chunks reconstructs the file. The single point of failure shifts from node honesty to key secrecy.

### Attacker Capabilities

| Attacker | Capability |
|----------|-----------|
| Malicious node operator (Eve) | Holds ciphertext only. Cannot read content. Can serve to anyone but it's meaningless without key. |
| Network observer | Sees encrypted chunk transfers. Learns file sizes but nothing about content. |
| Targeted attacker | Compromising all 3 holding nodes yields only ciphertext. Useless without key. |
| Key compromise | Full access to file content. Single point of failure. |

---

## Comparison Matrix

| Property | Deterministic + ACL | Opt-In | Client-Side Encryption |
|----------|-------------------|--------|----------------------|
| Guaranteed R=3 redundancy | Yes | No — market-driven | Yes (orthogonal to assignment) |
| Zero coordination | Yes | No — requires opt-in txs | Yes |
| Self-healing replication | Yes (MarketSync) | No — manual gap-fill | Yes (orthogonal) |
| Node can read stored data | Yes — plaintext | Yes — plaintext | No — ciphertext only |
| ACL bypass by modified binary | Possible | Possible | Irrelevant — data is encrypted |
| Metadata privacy | No — gossipsub leaks | No — worse (opt-in txs public) | Partial — sizes visible, content hidden |
| On-chain cost | Zero (pure computation) | High (tx per opt-in) | Zero (encryption is client-side) |
| Key management burden | None | None | High — Alice's responsibility |
| Access revocation | Weak (retroactive only) | Weak (same issue) | Strong (don't share key) |
| Sybil resistance | Strong (math assigns) | Weak (concentrates rewards) | N/A (orthogonal) |
| Public file support | Yes | Yes | Unnecessary — skip encryption |

---

## Recommended Approach

**Deterministic assignment (current) + mandatory client-side encryption for private files.**

### Why this combination

1. **Keep deterministic assignment.** It provides guaranteed redundancy, zero coordination, self-healing, no on-chain bloat, and Sybil resistance. These properties are too valuable to trade away.

2. **Add client-side encryption for private files.** When Alice sets a non-empty `access_list`, the client tool encrypts before chunking. The encryption key is derived from Alice's Ed25519 private key + a per-file nonce, so Alice can always re-derive it. Alice shares the key with Bob via a separate secure channel.

3. **Keep the off-chain ACL check as a bandwidth optimization.** Nodes still check `storage_getAccessList()` before serving — not for security (encryption handles that) but to avoid wasting bandwidth serving ciphertext to peers who can't decrypt it.

4. **For public files (empty access_list), skip encryption.** No key management overhead, no encryption/decryption cost. The data is intentionally open.

### Why not opt-in

It adds complexity, on-chain bloat, and coordination problems while solving none of the security issues that encryption solves. The only benefit (node economic choice) is marginal — nodes already earn proportionally under deterministic assignment since chunks are evenly distributed.

### Why encryption is the right layer

The fundamental problem is that nodes hold data on untrusted hardware. No amount of ACL checking, assignment logic, or on-chain rules can prevent a node operator from reading their own filesystem. Encryption is the only mechanism that makes data confidentiality independent of node honesty. Everything else is policy enforcement on an honor system.

### Implementation Notes

- **Algorithm:** XChaCha20-Poly1305 (authenticated encryption, 256-bit key, 192-bit nonce)
- **Crate:** `chacha20poly1305` in Rust
- **Key derivation:** `key = blake3(ed25519_seed ++ merkle_root_of_plaintext ++ "sumchain-file-key")` — deterministic from Alice's wallet seed, so she never needs to remember a separate password
- **Where it fits:** Wraps the existing chunker pipeline. `encrypt(file) → chunk(ciphertext) → hash → merkle_tree`. Decryption is the reverse on download: `fetch_chunks → reassemble_ciphertext → decrypt(key, ciphertext) → plaintext`
- **Priority:** Phase 5, after WAN discovery and production hardening. The ACL provides functional (not cryptographic) access control in the interim.
