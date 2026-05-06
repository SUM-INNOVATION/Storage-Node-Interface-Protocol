# Release checklist

Run these checks in order before tagging and shipping a SNIP release.
Anything that fails blocks the release; anything that warns goes in
[`CHANGELOG.md`](../CHANGELOG.md) under "Known issues."

> Examples below use placeholders for environment-specific values:
> `<internal-chain-release-tag>`, `<chain-rpc-host>`,
> `<known-v2-root>`, `<known-tx-hash>`. Substitute for your release
> environment. Real RPC URLs, validator hostnames, funded addresses,
> and internal chain commits MUST NOT appear in this file.

## 0. CI gate (one-time setup, then automated)

Every PR runs `make release-check` on `ubuntu-24.04` via
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml). The job
is named exactly `release-check (linux)` so a GitHub branch-
protection rule can require it by stable name.

One-time setup:

- [ ] In GitHub: **Settings → Branches → main** (Branch protection
      rule). Require status check `release-check (linux)` to pass
      before merging. Enable "Require branches to be up to date
      before merging" so the gate runs against the merge result,
      not a stale base.
- [ ] Confirm a freshly opened PR shows the `release-check (linux)`
      check as required (not "expected") — the rule only applies
      after at least one run completes on `main`.

What CI runs:

- `make release-check` — `fmt` + `lint-strict` + `test` + `build` +
  `audit-logs`. Same chain operators run locally.

What CI does NOT run:

- **Live-chain smoke** (`make smoke RPC=…`) — manual only. CI has
  no chain RPC URL and no chain secrets. Run before tagging
  (see § 4 below).
- Release artifact builds, coverage, macOS matrix — explicitly out
  of scope for the merge gate. Add later in separate workflows
  once the linux gate is stable in production.

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
      row holds an actual SHA. The SHA MUST be from a chain
      history that contains no committed signing material; if
      chain ops rewrote history to scrub secrets, confirm the
      rewrite landed before pinning.
- [ ] The fixture tests pass without modification on the pinned
      SHA's regenerated mirror types:
      ```bash
      cargo test -p sum-node tx_builder rpc_client
      cargo test -p sum-types
      ```
      Any diff in fixture bytes is a wire-format change and MUST be
      a separate commit referencing the new chain SHA (chain-team
      coordination required).
- [ ] Chain team has reported a green run of their own V2 wire-
      fixture suite on the pinned SHA (e.g.
      `cargo test -p sumchain-primitives --test v2_wire_fixtures`).
      Both sides green means the V2 wire shape is confirmed-stable
      on this commit.
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

> **Mirror is runnable at the pinned chain SHA.** The
> chain-side compose preset at
> `deploy/snip-local-mirror.yaml` brings up a self-
> bootstrapping single-validator devnet (validator key
> generated into a Docker named volume on first boot; no
> signing material in the repo). The SNIP-side **WS2 E2E
> suite** that drives this mirror end-to-end through the full
> Phase 4 lifecycle is the next workstream — until it lands,
> the mirror is verified via `make smoke` only.

Bring up (from the chain checkout):

```bash
git checkout 5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0
docker-compose -f deploy/snip-local-mirror.yaml up -d --build
```

Health check (read-only, no tx):

```bash
make smoke RPC=http://localhost:8545
# Expect: V2 state ENABLED_FROM_GENESIS, finalized height advancing
# (~2s block cadence). chain_id = 31337.
```

Stop / wipe (from the chain checkout):

```bash
docker-compose -f deploy/snip-local-mirror.yaml down       # preserve volume
docker-compose -f deploy/snip-local-mirror.yaml down -v    # wipe + regen keys
```

Optional fresh-genesis funded accounts: see
[`OPERATOR-RUNBOOK.md`](../docs/OPERATOR-RUNBOOK.md)
"Funded test accounts (optional, fresh-genesis only)". DO NOT
commit private keys for the funded addresses.

When WS2 ships:

```bash
cargo test --test e2e_lifecycle -- --include-ignored
```

Asserts: Public ingest/download, Private owner-only, Private
shared, share/revoke/update-access with finality boundary,
archive restart recovery, V1 legacy compatibility.
>
> Until then, releases gate on:
>
>   * `make release-check` (the linux CI gate, § 0).
>   * In-tree bincode v1 fixture tests (§ 3) — exhaustive for the
>     V2 wire surface SNIP submits / decodes.
>   * Live-chain read-only smoke (§ 4) — sanity-checks the target
>     RPC's V2 state without sending any tx.

When unblocked:

```bash
# Bring up the chain-ops-supplied local mirror per their updated
# instructions (exact command depends on the artifact they ship —
# could be `docker-compose up`, a binary, etc).
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
