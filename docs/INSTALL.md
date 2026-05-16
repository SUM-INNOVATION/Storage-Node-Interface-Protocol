# Installing SNIP

This document describes how to install the `sum-node` and
`e2e-helper` binaries from a published SNIP release.

Two install paths are documented in order of safety. **Read the
manual-verify path first** — the curl-pipe-sh one-liner is a
convenience that does the same steps, but you should understand
what it does before piping arbitrary scripts into your shell.

> Prebuilt binaries are published only for **Linux x86_64** in the
> `v0.4.x` line. Every other supported platform cell builds from
> source. See [`PLATFORM-SUPPORT.md`](PLATFORM-SUPPORT.md).

## Manual-verify path (recommended for first install)

For a target version `vX.Y.Z` (e.g. `v0.4.0`):

```bash
VERSION=v0.4.0
PLATFORM=linux-x86_64
BASE=https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/${VERSION}

# 1. Download the tarball and checksums.
curl -fsSLO "${BASE}/snip-${VERSION}-${PLATFORM}.tar.gz"
curl -fsSLO "${BASE}/SHA256SUMS"

# 2. Verify SHA256.
sha256sum --check --ignore-missing SHA256SUMS

# 3. Extract.
tar xzf "snip-${VERSION}-${PLATFORM}.tar.gz"

# 4a. Install into ~/.local/bin (no sudo, single-user).
mkdir -p "$HOME/.local/bin"
mv "snip-${VERSION}-${PLATFORM}/bin/sum-node"   "$HOME/.local/bin/"
mv "snip-${VERSION}-${PLATFORM}/bin/e2e-helper" "$HOME/.local/bin/"

# 5. Confirm.
sum-node --version
```

If `~/.local/bin` is not on your `$PATH`, add it to your shell rc:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc   # or ~/.zshrc
```

To install system-wide, replace step 4a with:

```bash
# 4b. Install into /usr/local/bin (system-wide, requires sudo).
sudo mv "snip-${VERSION}-${PLATFORM}/bin/sum-node"   /usr/local/bin/
sudo mv "snip-${VERSION}-${PLATFORM}/bin/e2e-helper" /usr/local/bin/
```

## Curl-pipe convenience path

The install script attached to each release performs the same
steps. It requires you to pass `--version` explicitly — there is
no implicit "latest." The script refuses to run on anything other
than Linux x86_64 and does not invoke `sudo` itself.

User-local install (default `$HOME/.local/bin`):

```bash
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sh -s -- --version v0.4.0
```

System-wide install (user-supplied sudo, the script does not
auto-elevate):

```bash
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sudo sh -s -- --version v0.4.0 --prefix /usr/local
```

What the script does, in order:

1. Refuses to run unless `--version vX.Y.Z` (or `vX.Y.Z-rcN`) is
   passed. There is no `--latest`.
2. Refuses to run on anything other than `Linux x86_64`.
3. Downloads `snip-${VERSION}-linux-x86_64.tar.gz` and `SHA256SUMS`
   from the release page into a `mktemp -d` directory.
4. Verifies the tarball against `SHA256SUMS` with
   `sha256sum --check --ignore-missing`.
5. Extracts and moves `sum-node` and `e2e-helper` into
   `${PREFIX}/bin` (default `$HOME/.local/bin`).
6. Warns if the install dir is not on `$PATH`.

The script itself lives at [`scripts/install.sh`](../scripts/install.sh)
in the repository. The release-asset copy is bit-identical to the
copy at the matching tag.

> The install command always references the release-asset URL
> (`.../releases/download/vX.Y.Z/install.sh`), never `main`. The
> tag's installer is what installs the tag's binaries.

## Build from source

If your platform does not have a prebuilt tarball, or you prefer
to build from source on principle, follow the per-environment
recipe in [`PLATFORM-SUPPORT.md`](PLATFORM-SUPPORT.md). The short
version on Linux:

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"

git clone https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol.git
cd Storage-Node-Interface-Protocol
git checkout v0.4.0            # or target release tag
make release-check
```

The built binaries land at `target/release/sum-node` and
`target/release/e2e-helper`.

## Tarball layout

```
snip-vX.Y.Z-linux-x86_64/
  bin/
    sum-node
    e2e-helper
  README.md
  CHANGELOG.md
  LICENSE-MIT
  LICENSE-APACHE
```

`SHA256SUMS` is a sibling asset on the release page (not inside
the tarball). The tarball itself is single-flat-root: it expands
into one top-level directory matching its basename without
extension.

## Security notes

- The trust root is the GitHub Releases page over TLS. The
  installer (and the manual path) verify the tarball against the
  `SHA256SUMS` file that ships in the same release. This protects
  against transport corruption, not against a malicious release
  uploader. If your threat model requires stronger guarantees,
  build from source at the matching tag.
- `v0.4.x` does not ship code-signed binaries, notarized macOS
  bundles, Sigstore attestations, or a PGP-signed `SHA256SUMS`.
  These are deferred to a future packaging workstream. See
  [`PLATFORM-SUPPORT.md`](PLATFORM-SUPPORT.md) "Not planned for
  v0.4.x" for the full list of deferred packaging items.
- The installer does not run `sudo` for you. To install to a
  system path, run the curl-pipe under your own `sudo`.
- Always pass `--version` explicitly. There is no `--latest` flag
  for `v0.4.x`. If you want to roll forward, change the version
  string yourself after reading the release notes.
- The installer downloads through `curl -fsSL --proto '=https'
  --tlsv1.2` and stages everything under `mktemp -d` with a
  cleanup trap. It will not extract to or overwrite anything
  outside the install prefix.
