# Quickstart: client

Fastest path to upload a small file, then download it, using
prebuilt binaries on Linux x86_64. For every other platform build
from source per [`install.md`](install.md) and skip step 1.

You will need:

- An Ed25519 seed hex file (32 random bytes, `chmod 600`).
- Enough Koppa on the seed's L1 address to pay a
  `RegisterFilePendingV2` fee deposit plus tx fees.
- The mainnet RPC URL (`https://rpc.sumchain.io`) or a running
  local mirror (`http://localhost:8545`).

## 1. Install

```bash
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sh -s -- --version v0.4.0
sum-node --version
```

See [`install.md`](install.md) for the manual-verify path and for
non-Linux-x86_64 environments.

## 2. Configure

```bash
# Generate a seed (skip if you already have one).
openssl rand -hex 32 > ~/.sumnode/mykey.seed.hex
chmod 600 ~/.sumnode/mykey.seed.hex

# Environment vars for the rest of this walkthrough.
export SUM_KEY_FILE=~/.sumnode/mykey.seed.hex
export SUM_RPC_URL=https://rpc.sumchain.io   # or http://localhost:8545 for local-mirror
```

Fund the address before continuing:

```bash
# Print the address for your seed.
sum-node --version   # sanity check
cargo run --release -p sum-node --bin e2e-helper -- \
    l1-address --seed-hex "$(cat $SUM_KEY_FILE)"
```

Send Koppa to the printed address out of band (chain team faucet,
DEX, etc.), then verify:

```bash
cargo run --release -p sum-node --bin e2e-helper -- \
    balance --rpc-url "$SUM_RPC_URL" --address <your-l1-address>
```

## 3. Upload a Public file

```bash
sum-node --client \
    --rpc-url "$SUM_RPC_URL" \
    --chain-id 1 \
    ingest-v2 ./hello.txt --visibility public
```

Stable stdout on success:

```text
merkle_root: 0x<hex>
lifecycle: Active
```

Record the `merkle_root` — you will need it to download.

For local-mirror substitute `--chain-id 1337`. Both values are
required today for the paths that consume the CLI value — see
[`../reference/config-flags.md`](../reference/config-flags.md)
"Chain ID safety."

## 4. Download

```bash
sum-node --client \
    --rpc-url "$SUM_RPC_URL" \
    download <merkle-root-hex> --output ./out.txt
diff hello.txt out.txt   # should be empty
```

`--key-file` is only required when downloading a Private file.

## Next

- Private files, share / revoke / update-access:
  [`../client/upload-and-download.md`](../client/upload-and-download.md).
- Key hygiene: [`../client/key-management.md`](../client/key-management.md).
- Full CLI: [`../reference/cli.md`](../reference/cli.md).
