# Archive operator quickstart

Bring up a storage (archive) node: register on chain, start serving, and begin
answering proof-of-retrievability challenges. This is the "N1 through N10" path
from the [complete process walkthrough](../reference/CMPLT-PROC.md). An archive
node stakes Koppa, holds file chunks assigned to it, and earns Koppa by proving
retrievability on challenge.

This is a quickstart, not a production runbook. For host prerequisites, fleet
coordination, monitoring, and failure triage, use
[`OPERATOR-RUNBOOK.md`](../operations/OPERATOR-RUNBOOK.md) and
[`MAINNET-BRINGUP.md`](../operations/MAINNET-BRINGUP.md).

## Prerequisites

- A Linux x86_64 host (archive mode is Linux-first; see
  [`PLATFORM-SUPPORT.md`](../reference/PLATFORM-SUPPORT.md)).
- `sum-node` installed ([`INSTALL.md`](../reference/INSTALL.md)).
- An Ed25519 key file, `chmod 600`, backed up off-machine. This is the keys to
  the kingdom: it is your stake account and your node identity.
- A funded account with enough Koppa to cover the stake (default
  `1,000,000,000` base units = 1 Koppa) plus transaction fees.
- The RPC URL of a SUM Chain node (mainnet: `https://rpc.sumchain.io`).

## 1. Register as an archive node

```bash
sum-node \
    --key-file node.hex \
    --rpc-url https://rpc.sumchain.io \
    register-node --stake 1000000000
```

This submits `NodeRegistry::Register(ArchiveNode { stake })`, waits for
finality, and prints the transaction hash and the block it landed in. The chain
ID is read live from RPC so you cannot mis-flag the transaction against the
wrong network. Override `--stake` only if your chain genesis pins a different
minimum.

Confirm the registration took:

```bash
sum-node --rpc-url https://rpc.sumchain.io register-node   # idempotent-safe recheck is via the runbook
```

The canonical way operators verify is `storage_getActiveNodes` /
`storage_getNodeRecord` (see [`RPC-API.md`](../reference/RPC-API.md)); the
[runbook](../operations/OPERATOR-RUNBOOK.md) has the exact query.

## 2. Start serving

```bash
sum-node \
    --key-file node.hex \
    --rpc-url https://rpc.sumchain.io \
    --enable-wan \
    listen
```

`listen` runs indefinitely. It serves chunk requests, enforces access-control on
private files, and starts the background workers: the PorWorker (polls for and
answers PoR challenges) and the MarketSync worker (V1-legacy self-heal). This is
the command you put under a process supervisor.

Add `--enable-wan` for a public host so the node is discoverable beyond the LAN
via Kademlia DHT. If the node is behind NAT, see the NAT-traversal notes in the
[runbook](../operations/OPERATOR-RUNBOOK.md) (AutoNAT, Circuit Relay v2, DCUtR).
Pin `--udp-port` to a stable value if you need the node reliably dialable over
QUIC.

Run it under systemd (sketch):

```ini
[Service]
Environment=SUM_KEY_FILE=/etc/sum/node.hex
Environment=SUM_RPC_URL=https://rpc.sumchain.io
Environment=SUM_ENABLE_WAN=1
Environment=SUM_PROFILE=production
ExecStart=/usr/local/bin/sum-node listen
Restart=always
```

Keep `SUM_PROFILE=production` (the default). It fails closed on every uncertain
access path. Never run `dev` in a real deployment.

## 3. Watch it work

The node's observability is log-based today (there is no metrics HTTP endpoint;
see [`MONITORING.md`](../operations/MONITORING.md)). Run with `RUST_LOG=info`
and watch for:

- `PoR worker started` at boot, with your address and poll interval.
- `active challenges found` when the chain issues a challenge targeting you.
- `PoR proof submitted` when you answer one. This is the heartbeat that means
  the node is holding data and earning.
- `PoR poll failed — backing off` on RPC trouble. Occasional is fine; sustained
  means the node cannot reach the chain.

## How you earn, and how you lose

Every `CHALLENGE_INTERVAL_BLOCKS` the chain challenges a random active archive
to prove it holds a random chunk. Answer with a valid Merkle proof before the
challenge TTL (50 blocks) and you are paid from the file's fee pool and your
stake is preserved. Miss it and the chain slashes a percentage of your stake and
marks you `Slashed`, removing you from service. Expired challenges are processed
at the start of each block, before user transactions, so there is no last-second
escape. The full mechanism, including why the target may be a node the
assignment algorithm did not pick, is in
[`CMPLT-PROC.md`](../reference/CMPLT-PROC.md) (Steps 5 through 7).

## See also

- [`OPERATOR-RUNBOOK.md`](../operations/OPERATOR-RUNBOOK.md): run, monitor, recover
- [`MAINNET-BRINGUP.md`](../operations/MAINNET-BRINGUP.md): full fleet bring-up
- [`MONITORING.md`](../operations/MONITORING.md): what to watch and alert on
- [`CLI.md`](../reference/CLI.md): every flag and subcommand
