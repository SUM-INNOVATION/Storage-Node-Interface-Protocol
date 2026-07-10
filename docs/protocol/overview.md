# Protocol overview

SNIP is a native decentralized storage protocol for the SUM Chain.
The chain acts as the cryptographic ledger — recording only merkle
roots, access lists, and fee pools — while actual file bytes live
off-chain in a libp2p peer-to-peer mesh of storage nodes ("archives").
Archives earn Koppa (the chain's native currency) by proving they
hold assigned chunks; a deterministic assignment algorithm places
`R = 3` replicas of each chunk on distinct archives, and both the
chain and every archive compute the same assignment independently
from on-chain state.

There are no smart contracts, no IPFS dependency, and no separate
storage token — storage economics settle directly at the consensus
layer.

## Actors

- **Validators.** Run the SUM Chain. They keep consensus, store
  file metadata (never bytes), execute transactions, and issue
  Proof of Retrievability (PoR) challenges. They do not hold any
  file data.
- **Archives.** Run `sum-node listen`. They stake Koppa and are
  registered as `ArchiveNode` on chain. Each archive holds a
  deterministically-assigned subset of the network's chunks on
  local disk, responds to peer chunk requests, enforces ACLs on
  every serve, and answers PoR challenges to earn payouts. Archives
  can be slashed for failing to answer a challenge in time.
- **Clients (file owners).** Run `sum-node --client ingest-v2` and
  `sum-node --client download`. They pay storage fees to register
  files on chain, push encrypted or plaintext chunks to the assigned
  archives, and retrieve files by merkle root. Clients do not
  stake, do not run `listen`, and do not answer PoR challenges.
- **Clients (file readers).** Recipients of a file's access grant.
  For Public files, "everyone" is a reader. For Private files, only
  addresses on the access list can decrypt the manifest and pull
  chunks; the archives serve only requests that match a chain-side
  access entry.

## Two protocol versions

Two wire protocols coexist on the same libp2p swarm:

- **V2 (`/sum/storage/v2`)** is the chain-canonical path on mainnet
  under chain plan v3.2. Every push carries an inline Merkle proof;
  each archive attests coverage on chain via `AcceptAssignmentV2`;
  files activate only when a chain-computed coverage predicate
  passes. V2 supports Public and Private files, per-recipient
  encryption via X25519-wrapped `K_file` bundles, and typed
  recovery paths (`resume`, `abandon`).
- **V1 (`/sum/storage/v1`)** is preserved for backwards
  compatibility with files registered before chain plan v3.2. V1
  files have no per-push proof, no chain-recorded coverage bitmap,
  and rely on the `MarketSyncWorker` self-healing loop plus V1
  hash + linear-probe assignment. New files should register on V2.

Wire-format specifics are pinned in
[`../reference/chain-compat.md`](../reference/chain-compat.md). The
per-crate responsibilities are laid out in
[`../architecture/crates.md`](../architecture/crates.md).

## Core protocol properties

- **Chunk size.** 1 MiB (`1_048_576` bytes). A file of size `S` has
  `C = ceil(S / 1_048_576)` chunks; the last chunk may be
  partially full.
- **Chunk address.** BLAKE3 hash of the chunk bytes, wrapped as a
  self-describing CIDv1.
- **File identity.** Merkle tree over chunk hashes; the root is the
  file's identity on chain and in all client-facing commands.
- **Replication factor.** `R = 3` on mainnet. Verified at runtime
  via `chain_getChainParams.assignment_replication_factor`.
- **Assignment algorithm.** V2 uses rendezvous hashing with domain
  separation ("`sumchain SNIP-V2 chunk-assignment v1`") over
  `(merkle_root, chunk_index, archive_address)`. Every participant
  computes the same output from the same on-chain snapshot; there
  is no coordinator. See
  [`crates/sum-store/src/assignment_v2.rs`](../../crates/sum-store/src/assignment_v2.rs).
- **Retention enforcement.** V2 relies on chain-issued PoR
  challenges and slashing. Archives that fail to answer within
  `CHALLENGE_TTL_BLOCKS` lose a percentage of stake and flip to
  `Slashed`.

## Where to go next

- **Story-mode walkthrough** — one file, one client, one archive
  fleet, step-by-step: [`lifecycle.md`](lifecycle.md).
- **V2 state machine and recovery paths** —
  [`v2-state-machine.md`](v2-state-machine.md).
- **How challenges are targeted and answered** —
  [`proof-of-retrievability.md`](proof-of-retrievability.md).
- **Crate map** — [`../architecture/crates.md`](../architecture/crates.md).
- **Chain wire compatibility** —
  [`../reference/chain-compat.md`](../reference/chain-compat.md).
