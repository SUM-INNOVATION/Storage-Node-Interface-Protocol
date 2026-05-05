# Privacy audit

Tabular threat → mitigation → guard mapping for the SNIP node. Each
row names exactly one observable threat, the location of the
mitigation in source, and the test (or audit script) that pins it.
A row without a pinning guard is a row that will rot.

This doc is the artifact a security reviewer reads. Rows are added
as new threats are identified or new mitigations introduced; rows
are NOT deleted (move to "Retired threats" instead, with the commit
that retired them).

> This doc covers the SNIP **protocol and crate-level** behaviors
> that pin privacy guarantees. Environment-specific values (RPC
> hosts, validator/archive hostnames, real keys, real addresses,
> deployment topology) MUST NOT appear here; those belong in a
> private operator notebook outside this repo.

## Active threats

| #   | Threat                                                                                                  | Mitigation (location)                                                              | Pinning guard                                                                                   |
|-----|---------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------|
| 1   | Logs leak `K_file` plaintext                                                                            | No `info!`/`warn!`/`debug!`/`println!`/`eprintln!` formats `k_file` or `K_file`.   | [`scripts/audit-logs.sh`](../scripts/audit-logs.sh) (WS4) — grep guardrail in CI                 |
| 2   | Logs leak Ed25519 seed                                                                                  | Seed is read from `--key-file`, derived once, never formatted into a log macro.    | Audit guardrail (token: `seed`); manual: only peer ID + L1 address logged at startup            |
| 3   | Logs leak X25519 secret                                                                                 | Secret is wrapped in `Zeroizing<[u8; 32]>` immediately after derivation.            | Audit guardrail (token: `x25519_secret`); type system enforces no `Display`                     |
| 4   | Logs leak per-recipient encrypted key bundle (`encrypted_key_bundle` hex)                                | Bundle hex is parsed via [`parse_bundle_hex`](../crates/sum-node/src/download_private.rs) and never emitted. | Audit guardrail (tokens: `bundle_hex`, `encrypted_key_bundle`)                                  |
| 5   | Logs leak Private chunk plaintext                                                                       | Plaintext exists only inside `decrypt_and_verify_chunk` return; never logged.       | Audit guardrail (`plaintext` outside hash contexts)                                             |
| 6   | Logs leak decrypted manifest (file_name, chunk CIDs)                                                    | Decrypted `DataManifest` is logged at INFO only as `chunks` count + `total_size`.   | [`main.rs`](../crates/sum-node/src/main.rs) manifest log site; manual review confirms shape    |
| 7   | Encrypted key bundles only ever appear on-chain or in transit between owner and recipient                | `K_file` derivation lives in `sum-crypto`; chain stores only per-recipient wraps; serving path never persists unwrapped key material. | [`crates/sum-crypto`](../crates/sum-crypto) tests + access-list contract tests                   |
| 8   | V2 Private ACL fails open on chain RPC error                                                            | Production profile: any RPC error in ACL evaluation returns a typed RPC error (not "no access"). Fail-closed by default. | Behavioral fail-closed unit test (WS4) + manual review of [`access.rs`](../crates/sum-node/src/access.rs) |
| 9   | V1 fallback path silently downloads a Private V2 file                                                   | `run_download` routes to `run_download_private` only when `storage_getFileInfoV2` returns a Private V2 row. V1 path is taken only for V1/legacy rows. | V1-can't-serve-V2-Private unit test (WS4)                                                       |
| 10  | Revoked recipient can still fetch chunks post-revocation (chain-finalized)                              | Chain-side ACL check on every pull; once `RemoveAccessV2` finalizes, peer no longer appears in the access list. | [`v2_lifecycle.rs:W12-3`](../crates/sum-node/tests/v2_lifecycle.rs) (`v2_manifest_pull_handle_acl_denied_returns_access_denied`) + local-mirror E2E (WS2) |
| 11  | Expired recipient (`expires_at < finalized_height`) can still fetch                                     | [`check_access_expiry`](../crates/sum-node/src/download_private.rs) enforced strict-`>` rule against finalized head. | `check_access_expiry_strict_greater_rule` test                                                   |
| 12  | Temp ciphertext file from Private ingest persists on abort                                              | [`PrivateArtifacts._ciphertext_temp`](../crates/sum-node/src/ingest_v2.rs) is `tempfile::NamedTempFile`; drop on success/abort cleans up. | Temp-cleanup unit test (WS4)                                                                     |
| 13  | Public files reject world-read peers                                                                    | Public path explicitly takes the V1/Public branch at chain row level; no Private ACL applied. | Smoke: download a Public V2 row from a fresh peer with no access entry (WS2 / local-mirror)      |
| 14  | Forward secrecy on revoke (revoked party retains decryptable cached ciphertext)                          | **NOT mitigated.** Documented as Phase 5+. Operators warned in [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md). | N/A — known gap; revocation does NOT rotate `K_file`; for forward secrecy, revoke + re-ingest under a fresh `K_file`. |
| 15  | Chain returns ambiguous response that admits Private fetch                                              | [`access.rs`](../crates/sum-node/src/access.rs) treats RPC error as `V2NotEnabled`; production profile fails closed. | Manual review; behavioral fail-closed unit test (WS4)                                            |
| 16  | `v2_enabled_from_height: 0` accidentally treated as `None`                                              | `Option<u64>` distinguishes `Some(0)` ("from genesis") from `None` ("V2 disabled"). | [`rpc_types.rs`](../crates/sum-types/src/rpc_types.rs) `chain_params_v2_enabled_from_height_zero_is_some_zero` |
| 17  | Wrong-root manifest from a malicious archive poisons download                                           | [`fetch_manifest_v2`](../crates/sum-node/src/download_private.rs) validates inline (decrypt + chain root + Merkle); rejected manifest marks only that archive Failed. | `select_manifest_dispatch_*` tests                                                               |
| 18  | Chunk fetched from a non-assigned peer is accepted                                                       | [`fetch_all_ciphertext_chunks_v2`](../crates/sum-node/src/download_private.rs) only accepts `ShardReceived` from peers it dispatched to, scoped to the V2 deterministic assigned set. | Chunk-concurrency selector tests                                                                  |
| 19  | Tampered ciphertext (correct chain blake3 but corrupt AEAD ciphertext)                                  | `decrypt_and_verify_chunk` AEAD tag check fails decryption.                         | `decrypt_and_verify_chunk_tampered_ciphertext_rejects` test                                      |

## Privacy posture for operators

- The seed file (Ed25519 32 bytes) is the keys to the kingdom. Treat
  as a wallet private key. `chmod 600`. Off-machine backup. Never
  check in or paste into a chat.
- Public files are world-readable by design. Treat metadata
  (`file_name`, plaintext_size, chunk count) as public.
- Private files: chain stores wrapped key bundles; chain never sees
  `K_file`. Peers only get the wrapped bundle for their own access
  entry — chunks remain on assigned archives encrypted under
  `K_file`.
- Revocation removes the chain access entry but does NOT rotate
  `K_file`; revoked recipients with cached ciphertext + bundle can
  still decrypt past content. For forward secrecy, revoke +
  re-ingest under a fresh `K_file` (Phase 5+ for in-place rotation).

## Retired threats

(none yet)

## Audit cadence

- Each row checked at every release (see
  [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) §6).
- New threat? Add a row, name a guard, and reject the PR if the
  guard is "manual review."
