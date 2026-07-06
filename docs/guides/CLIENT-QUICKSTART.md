# Client quickstart

Publish a file to SUM Chain storage and download it back, end to end, as a file
user (not a storage-node operator). This is the "Alice and Bob" path from the
[complete process walkthrough](../reference/CMPLT-PROC.md): Alice publishes a
file, Bob retrieves it by its merkle root. You do not stake, register, or run
any storage infrastructure.

## Prerequisites

- `sum-node` installed (see [`INSTALL.md`](../reference/INSTALL.md)).
- An Ed25519 key file: 32 bytes, hex-encoded, one line. This is both your
  wallet and your network identity. Guard it like a wallet key (`chmod 600`).
- The RPC URL of a SUM Chain node (mainnet: `https://rpc.sumchain.io`).
- A funded account: publishing locks a fee deposit in Koppa, and every
  transaction pays a fee.

All client commands take the `--client` flag, which runs a single operation and
exits with none of the long-running node services.

## Publish a file

```bash
sum-node --client \
    --key-file alice.hex \
    --rpc-url https://rpc.sumchain.io \
    ingest-v2 ./report.pdf
```

This runs the full v2 lifecycle in one command: it registers the file on chain
(`RegisterFilePendingV2`), pushes each chunk to its R=3 deterministically
assigned archives, pushes the manifest, polls coverage, and activates the file
(`ActivateFileV2`). On success the file is `Active` on chain and downloadable.

The command prints the file's **merkle root**, a 64-character hex string. This
is the file's permanent address on the network. Save it, and share it with
anyone who should be able to download the file.

If the command fails after the on-chain registration, the file is left
`Pending`. You have two recovery options:

- **Retry:** `sum-node --client --key-file alice.hex resume <merkle_root> ./report.pdf`
- **Give up and reclaim the deposit:** `sum-node --client --key-file alice.hex abandon <merkle_root>`

Note that `abandon` is only admissible after the chain's activation grace period
(default 50 blocks) has elapsed since registration; the command pre-checks and
tells you the earliest height if you are too early.

## Download a file

Anyone with the merkle root can retrieve the file:

```bash
sum-node --client \
    --key-file bob.hex \
    --rpc-url https://rpc.sumchain.io \
    download 34a749...1b66 \
    --output ./report.pdf
```

`download` routes automatically based on the file's chain row (public or
private), fetches each chunk from one of its assigned archives, verifies every
chunk against the manifest hash, and rebuilds the Merkle tree to confirm the
reassembled file matches the chain's recorded root. If the root does not match,
the download reports an error rather than writing a corrupt file.

For a public file, any account can download. For a private file, you must be a
current, unexpired entry in the file's access list (see
[`PRIVATE-FILES.md`](PRIVATE-FILES.md)).

## Tuning

Both commands accept timeout and concurrency flags when you are on a slow link
or moving large files:

- `ingest-v2 --push-wait-secs`, `--manifest-push-wait-secs`, `--activation-wait-secs`
- `download --max-concurrent` (default 10), `--download-timeout-secs` (default 300)

See the [CLI reference](../reference/CLI.md) for the full flag list.

## What just happened

The chain never held your file. It recorded the file's merkle root, the fee
pool, and which archives are accountable for holding it. The bytes live on the
archive nodes in a peer-to-peer mesh, and their retrievability is enforced by
recurring proof-of-retrievability challenges with slashing. For the full
mechanism, read the [complete process walkthrough](../reference/CMPLT-PROC.md).

## See also

- [`PRIVATE-FILES.md`](PRIVATE-FILES.md): encrypted files with per-recipient access
- [`CLI.md`](../reference/CLI.md): every flag and subcommand
- [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md): the full protocol, step by step
