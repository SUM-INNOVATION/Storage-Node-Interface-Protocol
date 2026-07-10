# Quickstart: archive

Fastest path from "I have a VPS" to "I am registered on mainnet
and answering PoR challenges." This is the compressed happy-path
walkthrough. The comprehensive playbook is
[`../operator/mainnet-bringup.md`](../operator/mainnet-bringup.md);
detailed operational notes are in
[`../operator/runbook.md`](../operator/runbook.md).

**Use throwaway data only until every check in this quickstart
passes.**

## 1. Provision

- Linux x86_64 host (see [`../compatibility/platform-support.md`](../compatibility/platform-support.md)).
- Stable public UDP + TCP ports for QUIC and TCP fallback.
- Egress to `https://rpc.sumchain.io` (mainnet) or your local
  mirror endpoint.
- ~4 GB disk for the initial chunk store; scale with your expected
  assignment footprint.
- The archive's L1 account funded with at least the `--stake` value
  (`1_000_000_000` base units by default) plus enough for
  `register-encryption-key` + `register-node` fees.

## 2. Install

```bash
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sh -s -- --version v0.4.0
sum-node --version
```

Or build from source per [`install.md`](install.md).

## 3. Generate the archive's key

```bash
openssl rand -hex 32 > /secure/path/archive.seed.hex
chmod 600 /secure/path/archive.seed.hex
export SUM_KEY_FILE=/secure/path/archive.seed.hex
export SUM_RPC_URL=https://rpc.sumchain.io
```

Fund the address out of band (see the balance check in
[`quickstart-client.md`](quickstart-client.md) §2 for how to derive
and query it).

## 4. Read-only chain smoke

```bash
make smoke RPC="$SUM_RPC_URL" SMOKE_ARGS=--require-v2
```

Expect `smoke: ok` with `chain_id=1`, `R=3`,
`v2_enabled_from_height=Some(<mainnet-value>)`. If this fails,
stop and diagnose — no mainnet writes should proceed.

## 5. Register on chain

```bash
# X25519 encryption pubkey (required for Private V2 file shares).
sum-node --rpc-url "$SUM_RPC_URL" --chain-id 1 register-encryption-key

# Archive node registration + stake commitment.
sum-node --rpc-url "$SUM_RPC_URL" register-node --stake 1000000000
```

`register-node` reads `chain_id` live from RPC — no `--chain-id`
override needed. `register-encryption-key` consumes the CLI value
today, so pass `--chain-id 1` explicitly on mainnet until the
runtime fix lands. Both commands print `tx_hash:` and
`finalized_height:` on success.

## 6. Verify registration

```bash
cargo run --release -p sum-node --bin e2e-helper -- node-record \
    --rpc-url "$SUM_RPC_URL" \
    --address <archive-l1-address>
```

Expect `"role": "ArchiveNode"`, `"status": "Active"`, and
`"staked_balance": 1000000000`.

## 7. Start serving

```bash
sum-node \
    --rpc-url "$SUM_RPC_URL" \
    --chain-id 1 \
    --profile production \
    --enable-wan \
    --udp-port 4242 \
    listen
```

`--profile production` is mandatory on mainnet; `--profile dev`
MUST NEVER touch mainnet. `--chain-id 1` is required on the
current release for the listener's background `AssignmentAttestor`.

Run under `tmux` / `screen` for a first-run canary; move to
`systemd` or your supervisor of choice only after the archive has
survived at least one PoR cycle without incident.

## 8. Cross-references

- Full mainnet bring-up: [`../operator/mainnet-bringup.md`](../operator/mainnet-bringup.md).
- Monitoring / troubleshooting: [`../operator/monitoring.md`](../operator/monitoring.md).
- Runbook (steady-state operations): [`../operator/runbook.md`](../operator/runbook.md).
- Chain compatibility notes: [`../reference/chain-compat.md`](../reference/chain-compat.md).
- Chain ID safety notes: [`../reference/config-flags.md`](../reference/config-flags.md).
