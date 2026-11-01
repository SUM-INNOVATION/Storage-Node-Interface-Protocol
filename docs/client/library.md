# Client: library interface

`sum_node::client` is the supported way to upload and download files
from Rust instead of running the `sum-node` binary. It is a thin layer
over the code the CLI runs: the same V2 ingest pipeline as
`sum-node ingest-v2` and the same routing and orchestrators as
`sum-node download`. Chain transactions, wire messages, the manifest
and the merkle root are exactly those of the CLI. The CLI is
unchanged.

The CLI walkthrough is in
[`upload-and-download.md`](upload-and-download.md). Rustdoc on every
item: `cargo doc -p sum-node --open`, module `client`.

## Ingest

Build an `IngestPipeline` once and call `ingest` (Public) or
`ingest_private` (Private) with an `IngestInput`:

```rust
use std::sync::Arc;
use sum_node::client::{
    IngestInput, IngestParams, IngestParamsDefaults, IngestPipeline, L1RpcClient,
    MapPeerResolver, SumNet, identity,
};

let rpc = Arc::new(L1RpcClient::new(rpc_url));
// Live chain values; fail if the chain cannot be read (no fallback).
let params = IngestParams::from_chain_params(
    &rpc.chain_get_chain_params().await?,
    IngestParamsDefaults::default(),
);
let keypair = identity::keypair_from_seed(&seed)?; // seed: [u8; 32]
let my_addr = identity::l1_address_from_keypair(&keypair);
let net = Arc::new(SumNet::new(net_config, keypair).await?);
let pipeline = IngestPipeline::new(
    rpc, net.clone(), Arc::new(MapPeerResolver::new()), seed, my_addr, params,
);

let outcome = pipeline
    .ingest(IngestInput::Bytes { file_name: "episode-0017.bin", data: &episode })
    .await;
let root = outcome.merkle_root();       // Option<[u8; 32]>
let manifest = outcome.manifest();      // Option<&DataManifest>
net.shutdown().await?;
```

`net_config` is what the CLI builds from its network flags
(`--bootstrap-peer`, `--enable-wan`, `--mdns`, `--outbound-allow`, ...).

### Inputs

| `IngestInput` | What happens |
|---|---|
| `Path(&Path)` | The file is memory-mapped in place, as in the CLI. The manifest's `file_name` is the path's last component. |
| `Bytes { file_name, data }` | The bytes are written as `file_name` to a new temporary directory under `TMPDIR` (on Unix, readable only by the current user), ingested from there, and removed when the call returns. The manifest, and so the root, is the same as for a file of that name with the same contents. `file_name` must be one path component. |

Byte input costs one extra copy on disk for the duration of the call.
With `ingest_private`, that copy is the **plaintext**.

### Outcome, manifest and root

The return value is the pipeline's `IngestOutcome`, unchanged:
`Activated`, `PendingNeedsAction` (the file is registered and must be
resumed or abandoned) or `Failed` (nothing was registered). The
accessors read the manifest and root from any variant:

- `manifest()` is the manifest built from the input. It is `None`
  only when the call failed before chunking (for example a bad
  `file_name`, `R = 0`, V2 not enabled on the chain).
- `merkle_root()` is `manifest().merkle_root`: the root the file is,
  or would have been, registered under. It is present on a `Failed`
  outcome after chunking, when nothing is on chain.

For a Private file the root is over the ciphertext under a fresh key,
so it differs on every call and is known only after `ingest_private`
returns; `file_hash` and `total_size_bytes` describe the plaintext.

### Pending files

A `PendingNeedsAction` outcome is finished with the `IngestPipeline`
methods `resume(root, path)` or `abandon(root)`. To resume an ingest
that started from bytes, write the same bytes to a file with the same
`file_name` and pass that path.

## Download

```rust
use sum_node::client::{DownloadConfig, download_to_bytes, download_to_path};

let config = DownloadConfig::with_seed(rpc, net_config, seed)?;
let written = download_to_path(&config, root, Path::new("out.bin")).await?;
let (bytes, report) = download_to_bytes(&config, root).await?;
assert_eq!(report.manifest.file_name, "episode-0017.bin");
```

Each call reads the file's V2 row before any network activity, routes
on its visibility alone (Public, Private, or legacy when there is no
V2 row; an unknown visibility is refused), starts its own network
node, runs the CLI's orchestrator for that route with the CLI's
checks, and shuts the node down. A failed row lookup counts as "no V2
row", as in the CLI.

- `DownloadConfig::with_seed` downloads as the seed's account and is
  required for Private files. `DownloadConfig::ephemeral` uses a random
  network identity, like the CLI without `--key-file`, and refuses a
  Private file before any network activity.
- `max_concurrent` (default 10) and `timeout` (default 300 s) are the
  CLI's `--max-concurrent` and `--download-timeout-secs`.
- Public and legacy downloads go through the local chunk store
  (`store_dir`, resolved like `--store-dir`), hold its lease for the
  call, and leave the fetched chunks in it. A running archive node
  holds that lease, so use a separate store directory beside one.
- `download_to_bytes` downloads into a temporary directory and reads
  the file back: the whole file is held in memory, and for a Private
  file the plaintext is on disk until the call returns.

The report, `Downloaded`, carries the root, the route taken, the
manifest the output was assembled from (decrypted for a Private file),
and the transfer statistics (`None` for Private files).

## Using one network node

The pipeline reads the network's event stream, so run one operation
at a time per `SumNet`. Reuse one pipeline for consecutive ingests: its
`MapPeerResolver` keeps the archives it has identified. The download
functions start their own node per call.

## Not covered

- Preparing a file offline and publishing it later, resumable across
  restarts (SNIP #57 item 2). Each `ingest` call needs the chain and the
  network from registration to activation.
- Several files in one registration (SNIP #57 item 3). Every file costs one
  `RegisterFilePendingV2` and one `ActivateFileV2`, each waiting for
  finality.
