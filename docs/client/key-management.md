# Key management

A single Ed25519 seed is the client's or archive's entire identity
on SNIP. The same 32 bytes derive:

- the libp2p peer ID (P2P network identity);
- the L1 address (`blake3(pubkey)[12..32]`, base58 for display);
- the X25519 keypair used to receive Private V2 file shares
  (HKDF over the domain `snip-x25519-encryption-key-v1`).

Treat the seed file like a wallet private key.

## Generating

```bash
openssl rand -hex 32 > /secure/path/mykey.seed.hex
chmod 600 /secure/path/mykey.seed.hex
```

Any 32 uniformly-random bytes hex-encoded will do; `openssl rand
-hex 32` is the reference invocation and matches what the
runbook + mainnet bring-up examples use.

## Passing to the binary

Either flag or environment variable:

```bash
sum-node --key-file /secure/path/mykey.seed.hex ...
# or
export SUM_KEY_FILE=/secure/path/mykey.seed.hex
sum-node ...
```

Without `--key-file` the binary generates an ephemeral random
keypair and runs in dev mode. In dev mode PoR is disabled, V2
write commands are refused, and every log line names the
"ephemeral keypair" status at startup. **Never run without
`--key-file` in production.**

## Registering the X25519 pubkey

Before an address can receive a Private V2 file share (either as
the owner-only initial recipient or as a recipient added via
`share`), the address must register its X25519 pubkey on chain:

```bash
sum-node --key-file /secure/path/mykey.seed.hex \
    --rpc-url https://<chain-rpc-host> \
    --chain-id 1 \
    register-encryption-key
```

The X25519 keypair is derived deterministically from the Ed25519
seed. The private half never reaches the chain. Re-running the
command with the same seed is idempotent on chain but does submit
a fresh transaction on each invocation, so avoid unnecessary
repeats.

Pass `--chain-id 1` explicitly on mainnet on the current release
— see [`../reference/config-flags.md`](../reference/config-flags.md)
"Chain ID safety" for why this is required today.

## Rotation

`sum-node` supports overwriting the on-chain encryption key by
re-registering with a different seed's derived X25519 pubkey. This
is a **key rotation** on the identity level; it does not rotate
any per-file `K_file`. In particular:

- Files the operator previously received will still decrypt with
  the old seed's X25519 secret (via the wrapped bundle stored on
  chain at the time of the grant). The chain does not re-wrap
  bundles on rotation.
- For forward secrecy on file access, an owner must `revoke` +
  re-ingest under a fresh `K_file`. Row 14 of
  [`../security/privacy-audit.md`](../security/privacy-audit.md)
  is the pinning-guard record.

## Where the seed is (and is not) permitted

- **Permitted**: in the file at `--key-file`, in
  `Zeroizing<[u8; 32]>` in memory, in transaction signatures over
  the wire.
- **Not permitted anywhere**: in logs, in tracing output, in
  panics, in error messages, in filenames, in remote telemetry.
  The guard is [`scripts/audit-logs.sh`](../../scripts/audit-logs.sh),
  run on every `make release-check`.

## Operational hygiene

- Do not commit any seed to git.
- Do not paste any seed into chat, tickets, or PR descriptions.
- Off-machine backup: keep an offline copy on hardware you
  control. Losing the seed is losing every file that was ingested
  or received under it — there is no protocol-level recovery.
- On archive hosts, run under a dedicated OS user; do not share
  the seed with other services on the same host.
- Client and archive identities SHOULD be distinct addresses.
  Reusing an archive's seed as an ingest identity on the same
  host can produce local peer-identity collisions.

## Cross-references

- Upload / download flows: [`upload-and-download.md`](upload-and-download.md).
- Privacy pinning guardrails: [`../security/privacy-audit.md`](../security/privacy-audit.md).
- Threat model: [`../security/threat-model.md`](../security/threat-model.md).
- Runbook (archive-side key handling): [`../operator/runbook.md`](../operator/runbook.md).
