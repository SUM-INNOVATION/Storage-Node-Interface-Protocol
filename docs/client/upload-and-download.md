# Client: upload and download

End-user paths through the SNIP CLI. This document covers what
happens on `ingest-v2`, `download`, `share`, `revoke`, and
`update-access` — the commands a file owner or file reader
actually runs.

The archive operator's counterpart material is in
[`../operator/runbook.md`](../operator/runbook.md). The full CLI
reference is in [`../reference/cli.md`](../reference/cli.md).

## Prerequisites

- SNIP built or installed — see
  [`../getting-started/install.md`](../getting-started/install.md).
- An Ed25519 seed hex file (32 bytes, `chmod 600`), passed via
  `--key-file` or `SUM_KEY_FILE`. See
  [`key-management.md`](key-management.md).
- The seed's L1 address must have enough Koppa to pay the file's
  `fee_deposit` plus transaction fees.
- For Private files: the owner and each recipient must have already
  called `sum-node register-encryption-key` at least once (registers
  the derived X25519 pubkey on chain so `K_file` can be wrapped for
  them).

## Upload

### Public file

```bash
sum-node --client \
    --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    ingest-v2 ./photo.jpg --visibility public
```

The client:

1. Chunks the file locally into 1 MB pieces.
2. Signs and submits `RegisterFilePendingV2`, waits for finality.
3. Reads the active-node snapshot at `assignment_height`.
4. Pushes each chunk to its `R = 3` assigned archives in parallel,
   each push carrying an inline Merkle proof.
5. Pushes the CBOR manifest to each distinct assigned archive.
6. Polls `storage_getAssignmentCoverageV2` until
   `can_activate_now == true`.
7. Signs and submits `ActivateFileV2`, waits for finality.

Stable stdout on success:

```text
merkle_root: 0x<hex>
lifecycle: Active
```

### Private file

```bash
sum-node --client \
    --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    ingest-v2 ./diary.txt --visibility private
```

Same pipeline, but with encryption inserted before chunking. The
client generates a fresh `K_file` (ChaCha20-Poly1305 key), encrypts
every chunk + the manifest, and adds one `AccessEntryV2` per
recipient wrapping `K_file` for that recipient's registered X25519
pubkey. The owner is automatically added as a recipient.

Shared Private file:

```bash
sum-node --client \
    --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    ingest-v2 ./report.pdf --visibility private \
    --recipient <recipient-l1-address> \
    --recipient <recipient-l1-address>:<expires-at-height>
```

If any recipient has not registered their encryption pubkey on
chain, the command aborts before any state is written.

### Recovery

If `ingest-v2` exits with `lifecycle: Pending` — chunks pushed but
coverage or activation stalled — the file is recoverable:

- **`sum-node resume <merkle-root-hex> <path>`** — replay only the
  residual portion of the pipeline. Re-chunks `<path>` locally,
  asserts its computed root matches `<merkle-root-hex>`, then
  fills only what's missing.
- **`sum-node abandon <merkle-root-hex>`** — after
  `activation_grace_blocks` blocks past `created_at`, submit
  `AbandonFileV2` to release the deposit (minus the
  chain-configured `abandonment_fee_percent`).

Both commands read `chain_id` from the CLI value on the current
release, so pass `--chain-id 1` on mainnet until the runtime fix
lands (see [`../roadmap/roadmap.md`](../roadmap/roadmap.md)).

## Download

```bash
sum-node --client \
    --key-file /secure/path/reader.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    download <merkle-root-hex> --output ./out.bin --max-concurrent 4
```

The client:

1. Reads the chain row for `<merkle-root-hex>` (`storage_getFileInfoV2`).
2. Routes based on visibility:
   - Public → fetch manifest + chunks unencrypted.
   - Private → resolve the reader's access entry, unwrap the
     `K_file` bundle with the reader's X25519 secret, then fetch
     the encrypted manifest + ciphertext chunks and decrypt.
3. Fans out per-chunk pulls to the chunk's assigned-active archive
   set, bounded by `--max-concurrent`.
4. Verifies each chunk's BLAKE3 hash against the manifest.
5. Rebuilds the Merkle tree from the received chunk hashes and
   confirms it matches the on-chain merkle_root.
6. Writes the reassembled file to `--output`.

`--key-file` is required for Private files (the reader's seed is
needed to derive X25519 for the unwrap). Public files download
without a key.

## Share, revoke, update-access

Owner-only operations that mutate the file's access list on chain.
All three require the owner's key and read `chain_id` from the CLI
value on the current release; pass `--chain-id 1` explicitly on
mainnet.

### Share

```bash
sum-node --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    share <merkle-root-hex> \
    --recipient <recipient-l1-address>:<expires-at-height>
```

The owner locally recovers `K_file` from their own access bundle
on chain, wraps it for the new recipient's registered X25519
pubkey, and submits `AddAccessV2`. The chain never sees `K_file`.

Recipients without a registered encryption pubkey cause `share`
to abort BEFORE any transaction is submitted.

### Revoke

```bash
sum-node --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    revoke <merkle-root-hex> \
    --recipient <recipient-l1-address>
```

Removes the chain-side access entry. The revoked recipient no
longer appears in the file's access list; archives deny their
subsequent pulls at ACL check. **Does NOT rotate `K_file`.** For
forward secrecy — revoking access to already-cached ciphertext —
revoke and re-ingest under a fresh `K_file`. See row 14 of
[`../security/privacy-audit.md`](../security/privacy-audit.md).

### Update-access

```bash
sum-node --key-file /secure/path/owner.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    update-access <merkle-root-hex> \
    --recipient <recipient-l1-address>:<expires-at-height>
```

Updates a recipient's `expires_at` on chain. The encrypted key
bundle is preserved byte-for-byte; only the expiry changes. An
explicit directive is required: `<addr>:<height>` sets, `<addr>:none`
clears. A bare `<addr>` is rejected.

## Cross-references

- Key management: [`key-management.md`](key-management.md).
- Full CLI reference: [`../reference/cli.md`](../reference/cli.md).
- Chain compatibility (which RPC methods this uses):
  [`../reference/rpc-methods.md`](../reference/rpc-methods.md).
- Threat model: [`../security/threat-model.md`](../security/threat-model.md).
- Privacy pinning guardrails: [`../security/privacy-audit.md`](../security/privacy-audit.md).
