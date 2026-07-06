# Architecture overview

The shape of the SNIP codebase: what the five crates are, how they depend on
each other, and how a file moves through them. Start here before the per-crate
docs. This describes the storage-node side; the chain side lives in the
`sum-chain` repo, and the boundary between them is the RPC surface in
[`RPC-API.md`](../reference/RPC-API.md).

## The workspace

SNIP is a Cargo workspace on the Rust 2024 edition (minimum toolchain 1.85),
version `0.4.0-rc4`. Five crates, one of which holds the binaries:

| Crate | Kind | Responsibility |
|-------|------|----------------|
| [`sum-types`](../../crates/sum-types) | library (leaf) | Shared types: config, RPC response shapes, `DataManifest`, `CHUNK_SIZE`, storage types |
| [`sum-crypto`](../../crates/sum-crypto) | library (leaf) | AEAD chunk and manifest encryption, per-recipient key wrap, X25519 derivation. See [`SUM-CRYPTO.md`](SUM-CRYPTO.md) |
| [`sum-net`](../../crates/sum-net) | library | libp2p networking: transports, discovery, NAT traversal, the wire codec, identity derivation. See [`SUM-NET.md`](SUM-NET.md) |
| [`sum-store`](../../crates/sum-store) | library | Local chunk store, Merkle trees, deterministic assignment, garbage collection, manifest index, proof verification. See [`SUM-STORE.md`](SUM-STORE.md) |
| [`sum-node`](../../crates/sum-node) | binary | The daemon: orchestrates the other crates, owns the CLI, the workers, and the L1 RPC client |

`sum-node` builds two binaries: `sum-node` (the daemon) and `e2e-helper` (test
and operations helper).

## Dependency graph

```
                    sum-node  (binary: sum-node, e2e-helper)
                   /    |    \    \
                  /     |     \    \
           sum-crypto   |   sum-store
                        |     /   \
                     sum-net-/     \
                        |           \
                     sum-types <-----+
```

Read top-down: `sum-node` depends on all four. `sum-store` depends on `sum-net`
and `sum-types`. `sum-net` depends on `sum-types`. `sum-types` and `sum-crypto`
are leaves with no internal dependencies. The graph is acyclic and shallow,
which keeps the compile-time and reasoning surface small: the two crates that
define the data (`sum-types`) and the cryptography (`sum-crypto`) depend on
nothing internal, so their contracts are stable anchors for everything above.

### Notable external dependencies

| Concern | Crate(s) | Used by |
|---------|----------|---------|
| P2P networking | `libp2p` 0.55 | sum-net, sum-store |
| Async runtime | `tokio` 1.43 | sum-node, sum-net, sum-store |
| Hashing | `blake3` 1.5 | sum-store, sum-types, sum-net, sum-node |
| Signatures | `ed25519-dalek` 2.1 | sum-node |
| Key exchange | `x25519-dalek` 2.0 | sum-crypto |
| AEAD cipher | `chacha20poly1305` 0.10 | sum-crypto |
| KDF / hash | `hkdf` 0.12, `sha2` 0.10 | sum-crypto |
| Persistence (mmap) | `memmap2` 0.9 | sum-node, sum-store |
| Content addressing | `cid` 0.11, `multihash` 0.19 | sum-store |
| Wire encoding | `bincode` (V2 codec), `ciborium` (CBOR manifests) | sum-net, sum-store |
| L1 RPC | `reqwest` 0.12 | sum-node |
| CLI | `clap` 4.5 | sum-node |

## How a file moves through the crates

The end-to-end story lives in [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md); here
is the same flow expressed as which crate owns each step.

**Publish (ingest):**

1. `sum-store` chunks the file into 1 MiB pieces (`CHUNK_SIZE`), hashes each with
   BLAKE3, builds a Merkle tree, and produces a `DataManifest` (`sum-types`).
2. For a private file, `sum-crypto` encrypts each chunk and the manifest under
   keys derived from a fresh `K_file`, and wraps `K_file` per recipient.
3. `sum-node` registers the file on chain via the RPC client (`RegisterFilePendingV2`),
   then uses `sum-store`'s deterministic assignment to pick the R archives per
   chunk.
4. `sum-net` carries each chunk to its assigned archives over the `/sum/storage/v2`
   push protocol, with an inline Merkle proof; the receiving node validates the
   proof (`sum-store` verify) before writing to its `sum-store` chunk store.
5. `sum-node` polls coverage and submits `ActivateFileV2`.

**Serve and prove (archive):**

1. `sum-net` delivers inbound pull and push requests; `sum-node`'s ACL gate and
   push validator decide whether to serve or accept, using `sum-store`'s manifest
   index (`cid → merkle_root`) and assignment check.
2. `sum-node`'s PorWorker polls the chain, and on a challenge reads the chunk
   from the `sum-store` chunk store, builds a Merkle proof (`sum-store` merkle),
   signs it (`sum-net` identity / `ed25519`), and submits it via the RPC client.

**Retrieve (download):**

1. `sum-node` reads the file's chain row, routes public vs private, and rebuilds
   the same assignment (`sum-store`) to know which archives to ask.
2. `sum-net` fetches each chunk; `sum-node` verifies each against the manifest
   and, for private files, decrypts with `sum-crypto`; `sum-store` merkle
   confirms the reassembled root matches the chain.

## Why native handlers, not a VM

Every operation is a typed handler compiled into the node and the chain, not a
smart contract in a VM. The trade is deliberate: the assignment algorithm, the
Merkle proof format, and the wire codec are all consensus-critical and must be
bit-identical between SNIP and the chain. Keeping them as shared, versioned Rust
(`sum-store` assignment and merkle, mirrored on the chain side) makes that
equivalence testable in-process rather than mediated through a VM boundary. The
cost is that changing one is a coordinated change across both repos; see
[`V1-VS-V2.md`](V1-VS-V2.md) for how the protocol versions without breaking
history.

## See also

- [`SUM-CRYPTO.md`](SUM-CRYPTO.md), [`SUM-NET.md`](SUM-NET.md), [`SUM-STORE.md`](SUM-STORE.md): per-crate internals
- [`V1-VS-V2.md`](V1-VS-V2.md): the two protocol generations
- [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md): the end-to-end walkthrough
- [`LOCAL-DEV.md`](../guides/LOCAL-DEV.md): building and testing the workspace
