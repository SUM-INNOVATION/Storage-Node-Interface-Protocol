# Release checklist

Run these checks in order before tagging and shipping a SNIP release.
Anything that fails blocks the release; anything that warns goes in
[`CHANGELOG.md`](../../CHANGELOG.md) under "Known issues."

> Examples below use placeholders for environment-specific values:
> `<internal-chain-release-tag>`, `<chain-rpc-host>`,
> `<known-v2-root>`, `<known-tx-hash>`. Substitute for your release
> environment. Real RPC URLs, validator hostnames, funded addresses,
> and internal chain commits MUST NOT appear in this file.

## 0. CI gate (one-time setup, then automated)

Every PR runs `make release-check` on `ubuntu-24.04` via
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml). The job
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
exemption in the commit message and in [`Cargo.toml`](../../Cargo.toml)
`[workspace.lints.clippy]` with rationale.

## 3. Chain compatibility

- [ ] [`CHAIN-COMPAT.md`](../reference/chain-compat.md) "Pinned chain version"
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
> `sum-chain:deploy/snip-local-mirror.yaml` (in the chain
> repository, not this one) brings up a self-
> bootstrapping single-validator devnet (validator key
> generated into a Docker named volume on first boot; no
> signing material in the repo). The SNIP-side **WS2 E2E
> suite** that drives this mirror end-to-end through the full
> V2 lifecycle is the next workstream — until it lands, the
> mirror is verified via `make smoke` only.

Bring up (from the chain checkout):

```bash
git checkout 5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0
docker-compose -f deploy/snip-local-mirror.yaml up -d --build
```

Health check (read-only, no tx). After bringing the mirror up,
verify all three:

1. **chain_id returns `31337`** and finality is `"finalized"`:
   ```bash
   make smoke RPC=http://localhost:8545 SMOKE_ARGS=--require-v2
   # Expect: chain_id=31337, V2 state ENABLED_FROM_GENESIS.
   ```
2. **Block height advances** (~2s block cadence). The smoke
   line `chain_getBlockHeight ........... OK (finalized
   height=N, finality=finalized)` confirms; re-running smoke a
   few seconds later must show a higher N.
3. **Each WS2b role address has a non-zero balance** (only
   applies if you brought the mirror up with the funded-test-
   accounts overlay; see
   [`OPERATOR-RUNBOOK.md`](../operator/runbook.md)
   "Funded test accounts" for the per-role balance check
   loop).

A failure in any of the three is a hard release blocker.

Stop / wipe (from the chain checkout):

```bash
docker-compose -f deploy/snip-local-mirror.yaml down       # preserve volume
docker-compose -f deploy/snip-local-mirror.yaml down -v    # wipe + regen keys
```

Optional fresh-genesis funded accounts: see
[`OPERATOR-RUNBOOK.md`](../operator/runbook.md)
"Funded test accounts (optional, fresh-genesis only)". The
overlay file MUST use numeric balances (`{ "<base58>": <int>,
... }`), not string-encoded balances. Compose volume mounts MUST
be declared in YAML — `docker-compose` does not accept a `-v`
flag for runtime volumes; use a separate override file. If the
mirror has already been started, run
`docker-compose -f deploy/snip-local-mirror.yaml down -v`
before starting with the overlay (the overlay is fresh-genesis
gated). DO NOT commit private keys for the funded addresses.

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
- [ ] Each row in [`PRIVACY-AUDIT.md`](../security/privacy-audit.md) still has a
      pinning test reference; new threats added since last release
      have new rows.

## 7. Pre-final-release gates (release-candidate → final)

A release-candidate (`vX.Y.Z-rcN`) is **not** final production until
every gate below clears against that tag's commit. These gates exist
to catch hidden local-environment assumptions and chain-state
prerequisites that only surface on a fresh machine or against live
mainnet.

Step-by-step operator instructions for satisfying gates 2–4 against
live mainnet are in [`MAINNET-BRINGUP.md`](../operator/mainnet-bringup.md). This
section defines *what* must be true to promote; that guide describes
*how* to make it true.

- [ ] **Fresh-machine local-mirror E2E.** A different operator on a
      different machine clones the repo at the rc tag, generates
      keys via `e2e-helper generate-e2e-keys`, brings up the chain
      mirror per [`OPERATOR-RUNBOOK.md`](../operator/runbook.md), and
      runs the full WS2b suite:

      ```bash
      cargo test -p sum-node --test e2e_mirror -- \
          --ignored --test-threads=1 --nocapture
      # target: 11 passed; 0 failed
      ```

      The fresh-machine reproduction catches path / dependency /
      mirror-bring-up assumptions baked into the original developer's
      environment. A green run on the original machine is necessary
      but not sufficient.

- [ ] **Mainnet read-only smoke.** Against the live mainnet RPC at
      the chain commit listed in [`CHAIN-COMPAT.md`](../reference/chain-compat.md)
      "Mainnet pin / deployed chain":

      ```bash
      make smoke RPC=https://rpc.sumchain.io SMOKE_ARGS=--require-v2
      ```

      Confirms `chain_id`, V2 enablement state, and finality
      cadence match the values in CHAIN-COMPAT. A drift here means
      the chain advanced past what the rc was built against — bump
      the chain pin, re-run all gates.

- [ ] **At least 3 archive nodes registered + listening on mainnet.**
      Confirm via the `e2e-helper active-nodes-at-height` gate
      (reads finalized head + the active-nodes snapshot in one
      step; exit 2 when below threshold):

      ```bash
      cargo run --release -p sum-node --bin e2e-helper -- \
          active-nodes-at-height \
          --rpc-url https://rpc.sumchain.io \
          --height finalized \
          --require-archives 3
      # exit 0 ⇒ pre-flight cleared
      ```

      The chain plan's `assignment_replication_factor = 3` makes
      this a hard pre-flight: no V2 ingest can activate without
      three resolvable archive peers. Below 3, ingest registers on
      chain but stalls in `Pending` indefinitely.

- [ ] **First mainnet Public V2 ingest + download round-trip.** Use
      a throwaway file (operator personal data, NOT customer data)
      and verify the round-trip succeeds through the full
      `RegisterFilePendingV2` → push chunks → push manifest →
      `ActivateFileV2` → download → byte-identical reassembly path.
      This is the canonical "first real bytes" milestone; it is
      out-of-band today (no automated mainnet test). Record the
      tx hash and the merkle root in the release notes.

Only after **all four** gates clear does an `rcN` advance to a
final `vX.Y.Z` tag.

## 8. Tag and ship

- [ ] Bump version in [`Cargo.toml`](../../Cargo.toml)
      `[workspace.package].version`.
- [ ] Update [`CHANGELOG.md`](../../CHANGELOG.md):
  - Move "Unreleased" entries under a new
    `## [vX.Y.Z] — YYYY-MM-DD`.
  - Restate Phase / chain-version compatibility against
    `<internal-chain-release-tag>`.
- [ ] `git commit -m "chore: release vX.Y.Z"`.
- [ ] `git tag -a vX.Y.Z -m "vX.Y.Z"`.
- [ ] `git push && git push --tags`.

### 8a. Verify the prebuilt-binary draft release

Pushing the tag triggers
[`.github/workflows/release.yml`](../../.github/workflows/release.yml),
which builds the Linux x86_64 release binaries, packages them as
`snip-vX.Y.Z-linux-x86_64.tar.gz`, computes `SHA256SUMS`, and
uploads those plus `scripts/install.sh` to a **draft** GitHub
Release. The workflow never auto-publishes. A human must:

- [ ] Wait for the `release` workflow on the tag to finish green.
- [ ] Open the draft release in the GitHub UI and confirm the
      asset list contains exactly three files:
      `snip-vX.Y.Z-linux-x86_64.tar.gz`, `SHA256SUMS`,
      `install.sh`.
- [ ] On a clean Linux x86_64 host (or an Ubuntu 22.04 VM /
      container), exercise the **manual-verify path** end-to-end
      against the draft assets:

      ```bash
      # Authenticated download from the draft release.
      # Draft assets are NOT reachable via the unauthenticated
      # releases URL; use `gh release download` from a logged-in
      # gh CLI for the preflight.
      gh release download vX.Y.Z \
          --repo SUM-INNOVATION/Storage-Node-Interface-Protocol \
          --pattern 'snip-*-linux-x86_64.tar.gz' \
          --pattern 'SHA256SUMS'

      sha256sum --check --ignore-missing SHA256SUMS
      tar xzf snip-vX.Y.Z-linux-x86_64.tar.gz
      ./snip-vX.Y.Z-linux-x86_64/bin/sum-node --version
      ```

      Confirms tarball integrity, layout, and that the binary
      runs on a clean host. After publishing the release, the
      curl-pipe install line from the README/INSTALL becomes
      reachable; that is the next gate.
- [ ] Replace the auto-generated draft body with the matching
      `CHANGELOG.md` section + the install command.
- [ ] Manually publish the draft. For rc tags, keep the
      "pre-release" flag checked.
- [ ] After publishing, re-run the documented curl-pipe install
      line on a clean host as a final smoke:

      ```bash
      curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/vX.Y.Z/install.sh \
          | sh -s -- --version vX.Y.Z
      sum-node --version
      ```

      This is the line copy-pasted into the README; if it does
      not work after publish, no user can install vX.Y.Z.

## 9. Post-release

- [ ] Watch the first-deploy logs for any new warning/error patterns.
- [ ] Verify operator runbook still describes the shipped binary
      (CLI flag drift is the most common doc rot).
- [ ] If any fixture changed: ping chain team to confirm their CI
      sees the same bytes against the same internal release tag.
