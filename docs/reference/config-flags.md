# Configuration flags and environment variables

Canonical reference for global `sum-node` flags, their environment
variables, defaults, and the safety notes an operator needs to run
the binary against a live chain without burning fees on avoidable
mistakes.

Per-subcommand flags are listed in [`cli.md`](cli.md). Chain-facing
gates that the binary reads at runtime are listed in
[`../architecture/chain-integration.md`](../architecture/chain-integration.md).

## Global flags

| Flag | Env var | Default | Notes |
|---|---|---|---|
| `--key-file` | `SUM_KEY_FILE` | (none) | Ed25519 seed hex file. Without it the binary generates an ephemeral keypair and runs in dev mode with PoR and V2 write commands disabled. |
| `--rpc-url` | `SUM_RPC_URL` | `http://127.0.0.1:9944` | JSON-RPC endpoint on the SUM Chain node. |
| `--chain-id` | `SUM_CHAIN_ID` | `1337` | **See "Chain ID safety" below.** |
| `--attest-fee` | `SUM_ATTEST_FEE` | `1000000` (base units) | Per-tx fee used by V2 attestation paths. Must be ≥ chain `min_fee`. |
| `--por-poll-secs` | `SUM_POR_INTERVAL` | `10` | PoR challenge polling interval. |
| `--market-sync-secs` | `SUM_MARKET_SYNC_INTERVAL` | `30` | Market-sync polling interval. |
| `--gc-grace-secs` | `SUM_GC_GRACE` | `3600` | Grace period before deleting unassigned chunks. |
| `--client` | `SUM_CLIENT_MODE` | `false` | Client mode: `ingest` exits after confirmation, `listen` is refused. |
| `--enable-wan` | `SUM_ENABLE_WAN` | `false` | Enable Kademlia + TCP transport. |
| `--bootstrap-peer` | `SUM_BOOTSTRAP_PEERS` | (empty) | Multiaddrs for Kademlia bootstrap; repeatable or comma-separated. |
| `--tcp-port` | `SUM_TCP_PORT` | `0` (OS-assigned) | TCP listen port for WAN. |
| `--udp-port` | `SUM_UDP_PORT` | `0` (OS-assigned) | QUIC listen port. Pin for a reliably dialable node. |
| `--relay-server` | `SUM_RELAY_SERVER` | `false` | Advertise this node as a Circuit Relay v2 server. Requires `--enable-wan`. |
| `--profile` | `SUM_PROFILE` | `production` | `production` fails closed on uncertain ACL paths; `dev` relaxes them. **`dev` MUST NEVER touch mainnet.** |

## Chain ID safety

The workspace's `--chain-id` default is `1337`. **This value matches
no documented deployment:**

- Mainnet `chain_id` is `1`. See
  [`chain-compat.md`](chain-compat.md) "Mainnet pin / deployed chain."
- Local mirror `chain_id` is `1337`. See
  [`chain-compat.md`](chain-compat.md) "Pinned chain version."

### What consumes `--chain-id` today

The value is used at runtime as follows. **Rows marked "CLI value"
consume the flag directly and do not fall back to a chain RPC
query:**

| Code path | Source of `chain_id` |
|---|---|
| `register-node` (main.rs `run_register_node`) | **Live RPC** — reads `chain_id` from `chain_getChainParams`. Ignores `--chain-id`. |
| `ingest-v2`, `resume` (production profile) | Live RPC. `build_v2_ingest_params` calls `chain_getChainParams`; hard-fails when RPC errors. |
| `ingest-v2`, `resume` (dev profile only) | CLI value (fallback if `chain_getChainParams` errors). |
| `register-encryption-key` | CLI value. |
| `share`, `revoke`, `update-access` | CLI value. |
| `abandon` | CLI value. |
| `listen` — background `AssignmentAttestor` | CLI value. |

### Operational consequence

On mainnet, an operator who accepts the workspace default and
invokes `register-encryption-key`, `share`, `revoke`,
`update-access`, `abandon`, or `listen` without passing
`--chain-id 1` will sign transactions against `chain_id 1337`.
The chain does not recognise `1337` and will reject each such
transaction; the specific failure mode depends on chain execution
semantics, but the intended state change will not land. In the
listener's case this can leave the archive with chunks stored
locally that never advance to `Accepted` in
`storage_getAssignmentCoverageV2`, so ingest clients pushing to it
see files stall in `Pending`. The archive may also pay attestation
fees on each attempted `AcceptAssignmentV2`.

### Recommended usage

- **On mainnet**, always pass `--chain-id 1` explicitly on every
  `sum-node` invocation until this behavior is corrected in
  runtime code (see [`../roadmap/roadmap.md`](../roadmap/roadmap.md)
  "Runtime: read `chain_id` from RPC for every V2 tx-signing path").
- **On local mirror**, always pass `--chain-id 1337`.
- Prefer `SUM_CHAIN_ID` as an environment variable on hosts that
  run the binary from systemd / launchd, so a single misconfiguration
  cannot propagate silently across commands.

The `register-node` and production-profile `ingest-v2` paths are
safe against the default because they read `chain_id` from
`chain_getChainParams` at runtime. The other paths listed above
have not been migrated to that pattern yet.

## Profile

`--profile production` (default) fails closed on every ACL path
that cannot be resolved deterministically — RPC errors, unregistered
files, and unknown CIDs all deny. `--profile dev` relaxes those
paths so a developer working against a mock chain sees permissive
behavior. **Never run `--profile dev` against mainnet.** The dev
profile also enables the CLI-value fallback for `chain_id` in
`ingest-v2` and `resume`, which is desirable for local iteration
but unsafe against a real deployment.

## Cross-references

- Per-command flags: [`cli.md`](cli.md).
- Chain-side feature gates SNIP reads: [`../architecture/chain-integration.md`](../architecture/chain-integration.md).
- Pinned chain compatibility surface: [`chain-compat.md`](chain-compat.md).
- Live-chain first-run playbook: [`../operator/mainnet-bringup.md`](../operator/mainnet-bringup.md).
