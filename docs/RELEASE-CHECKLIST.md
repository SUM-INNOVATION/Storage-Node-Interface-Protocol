# Release checklist

Run these checks in order before tagging and shipping a SNIP release.
Anything that fails blocks the release; anything that warns goes in
[`CHANGELOG.md`](../CHANGELOG.md) under "Known issues."

> Examples below use placeholders for environment-specific values:
> `<internal-chain-release-tag>`, `<chain-rpc-host>`,
> `<known-v2-root>`, `<known-tx-hash>`. Substitute for your release
> environment. Real RPC URLs, validator hostnames, funded addresses,
> and internal chain commits MUST NOT appear in this file.

## 1. Workspace hygiene

- [ ] `git status` — no uncommitted changes, no untracked files
      except `target/` and editor scratch.
- [ ] On the release branch (typically `feature/...` merged into
      `main`).
- [ ] Workspace lives on a non-cloud-synced filesystem.

## 2. Local checks

```bash
make release-check
```

Equivalent to:

- `cargo fmt --check` — code is formatted.
- `cargo clippy --workspace --all-targets -- -D warnings` —
  zero clippy warnings under the workspace lint policy.
- `cargo test --workspace` — all tests green.
- `cargo build --release -p sum-node` — release binary builds.

If `lint-strict` fails on a warning class that can't be fixed in
this release, do NOT add a blanket `#[allow]`. Document the
exemption in the commit message and in [`Cargo.toml`](../Cargo.toml)
`[workspace.lints.clippy]` with rationale.

## 3. Chain compatibility

- [ ] [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md) "Pinned chain version"
      table reflects the internal release tag this release targets.
      The exact private chain commit is verified out-of-band by chain
      ops, NOT inlined here.
- [ ] The fixture tests pass without modification:
      ```bash
      cargo test -p sum-node tx_builder rpc_client
      cargo test -p sum-types
      ```
      Any diff in fixture bytes is a wire-format change and MUST be
      a separate commit referencing the new chain release tag
      (chain-team coordination required).
- [ ] V2-gate semantics tests still pass (regression guard for
      `Some(0)` ≠ `None`):
      ```bash
      cargo test -p sum-types chain_params_v2_enabled_from_height
      ```

## 4. Live-chain smoke (manual, read-only)

Live-chain smoke is **read-only by default**. Destructive /
write-path lifecycle tests require explicit operator approval and
double opt-in (env + flag) — never run them on a value-bearing
chain without that approval.

```bash
make smoke RPC=https://<chain-rpc-host>
```

Or via `e2e_helper` directly:

```bash
cargo run --release -p sum-node --bin e2e-helper -- \
    smoke --rpc-url https://<chain-rpc-host>
```

Asserts read-only:

- `chain_getChainParams` decodes; reports V2 state per
  `v2_enabled_from_height`:
  - `Some(0)` → "V2 enabled from genesis."
  - `Some(N)` for N>0 → "V2 enabled at finalized height N."
  - `None` → "V2 disabled on this chain."
- `chain_getBlockHeight(["finalized"])` returns finality.
- Optional, when env vars are set:
  - `account_getEncryptionPublicKey` for a known address.
  - `storage_getFileInfoV2` for `SNIP_SMOKE_KNOWN_ROOT=<known-v2-root>`.
  - `chain_getTransactionStatus` for `SNIP_SMOKE_KNOWN_TX=<known-tx-hash>`.

## 5. Local-mirror full E2E

> Local-mirror setup is provided by chain ops out-of-band. Use the
> artifact / runbook supplied for the target internal chain release.
> Until then, the mock-driven integration suite (`cargo test
> --workspace`) covers the SNIP-side logic.

When available:

```bash
# Bring up the chain-ops-supplied local mirror; exact command depends
# on the supplied artifact (e.g. `docker-compose up`, a binary, etc).
cargo test --test e2e_lifecycle -- --include-ignored
# Tear down per the supplied runbook.
```

Asserts: Public ingest/download, Private owner-only, Private shared,
share/revoke/update-access with finality boundary, archive restart
recovery, V1 legacy compatibility.

## 6. Privacy audit

- [ ] `scripts/audit-logs.sh` clean (lands in WS4). The script greps
      for forbidden tokens (`k_file`, `seed`, `x25519_secret`,
      `bundle_hex`, `encrypted_key_bundle`, raw `plaintext`) inside
      log-macro format strings.
- [ ] Each row in [`PRIVACY-AUDIT.md`](PRIVACY-AUDIT.md) still has a
      pinning test reference; new threats added since last release
      have new rows.

## 7. Tag and ship

- [ ] Bump version in [`Cargo.toml`](../Cargo.toml)
      `[workspace.package].version`.
- [ ] Update [`CHANGELOG.md`](../CHANGELOG.md):
  - Move "Unreleased" entries under a new
    `## [vX.Y.Z] — YYYY-MM-DD`.
  - Restate Phase / chain-version compatibility against
    `<internal-chain-release-tag>`.
- [ ] `git commit -m "chore: release vX.Y.Z"`.
- [ ] `git tag -a vX.Y.Z -m "vX.Y.Z"`.
- [ ] `git push && git push --tags`.

## 8. Post-release

- [ ] Watch the first-deploy logs for any new warning/error patterns.
- [ ] Verify operator runbook still describes the shipped binary
      (CLI flag drift is the most common doc rot).
- [ ] If any fixture changed: ping chain team to confirm their CI
      sees the same bytes against the same internal release tag.
