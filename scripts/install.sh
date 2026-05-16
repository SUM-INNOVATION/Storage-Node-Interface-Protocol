#!/usr/bin/env sh
# SNIP installer.
#
# Downloads a published SNIP release tarball from GitHub Releases,
# verifies it against SHA256SUMS, and installs sum-node + e2e-helper
# into a user-chosen prefix.
#
# Contract for v0.4.x:
#   - --version is REQUIRED. No implicit "latest".
#   - Linux x86_64 ONLY. Every other platform builds from source.
#   - This script does NOT run sudo for you. To install to a system
#     path, run the curl-pipe under your own sudo.
#
# Usage:
#   curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/vX.Y.Z/install.sh \
#       | sh -s -- --version vX.Y.Z
#
#   # System-wide (user-supplied sudo):
#   curl -fsSL .../install.sh \
#       | sudo sh -s -- --version vX.Y.Z --prefix /usr/local
#
# See docs/INSTALL.md for the manual-verify path that does the
# same steps by hand without piping curl into a shell.

set -eu

REPO="SUM-INNOVATION/Storage-Node-Interface-Protocol"
VERSION=""
PREFIX=""

usage() {
    cat >&2 <<'EOF'
Usage: install.sh --version vX.Y.Z [--prefix DIR]

  --version vX.Y.Z   (required) Pinned SNIP release tag, e.g. v0.4.0.
  --prefix DIR       Install to DIR/bin. Default: $HOME/.local/bin.
  -h, --help         Show this help.

Notes:
  - No 'latest'. The version must be passed explicitly.
  - Linux x86_64 only for v0.4.x. Other platforms: build from source
    (see docs/PLATFORM-SUPPORT.md).
  - This installer does NOT run sudo. To install to a system path,
    run the curl-pipe under sudo, e.g.:
      curl -fsSL <url>/install.sh \
          | sudo sh -s -- --version vX.Y.Z --prefix /usr/local
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            shift
            if [ $# -eq 0 ]; then
                printf 'error: --version requires a value\n' >&2
                exit 2
            fi
            VERSION="$1"
            shift
            ;;
        --version=*)
            VERSION="${1#--version=}"
            shift
            ;;
        --prefix)
            shift
            if [ $# -eq 0 ]; then
                printf 'error: --prefix requires a value\n' >&2
                exit 2
            fi
            PREFIX="$1"
            shift
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage
            exit 2
            ;;
    esac
done

if [ -z "$VERSION" ]; then
    printf 'error: --version is required (e.g. --version v0.4.0)\n' >&2
    usage
    exit 2
fi

case "$VERSION" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    v[0-9]*.[0-9]*.[0-9]*-rc[0-9]*) ;;
    *)
        printf 'error: --version must look like vX.Y.Z or vX.Y.Z-rcN (got: %s)\n' "$VERSION" >&2
        exit 2
        ;;
esac

OS="$(uname -s)"
ARCH="$(uname -m)"
if [ "$OS" != "Linux" ] || [ "$ARCH" != "x86_64" ]; then
    cat >&2 <<EOF
error: prebuilt binaries are only published for Linux x86_64 in $VERSION.
       detected: $OS $ARCH
       Build from source on this platform — see docs/PLATFORM-SUPPORT.md.
EOF
    exit 2
fi

PLATFORM="linux-x86_64"
TARBALL="snip-${VERSION}-${PLATFORM}.tar.gz"
CHECKSUMS="SHA256SUMS"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

if [ -z "$PREFIX" ]; then
    INSTALL_DIR="${HOME}/.local/bin"
else
    INSTALL_DIR="${PREFIX}/bin"
fi

for cmd in curl tar sha256sum mktemp mkdir mv chmod; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        printf 'error: required command missing: %s\n' "$cmd" >&2
        exit 1
    fi
done

mkdir -p "$INSTALL_DIR" 2>/dev/null || {
    printf 'error: cannot create install dir: %s\n' "$INSTALL_DIR" >&2
    printf '       (re-run under sudo if installing to a system path)\n' >&2
    exit 1
}

if [ ! -w "$INSTALL_DIR" ]; then
    printf 'error: install dir is not writable: %s\n' "$INSTALL_DIR" >&2
    printf '       (re-run under sudo if installing to a system path)\n' >&2
    exit 1
fi

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT HUP TERM

printf 'snip-install: target     %s\n' "$VERSION"
printf 'snip-install: platform   %s\n' "$PLATFORM"
printf 'snip-install: install to %s\n' "$INSTALL_DIR"

printf 'snip-install: downloading %s\n' "${BASE_URL}/${TARBALL}"
curl -fsSL --proto '=https' --tlsv1.2 -o "${TMP}/${TARBALL}" "${BASE_URL}/${TARBALL}"

printf 'snip-install: downloading %s\n' "${BASE_URL}/${CHECKSUMS}"
curl -fsSL --proto '=https' --tlsv1.2 -o "${TMP}/${CHECKSUMS}" "${BASE_URL}/${CHECKSUMS}"

printf 'snip-install: verifying SHA256\n'
if ! ( cd "$TMP" && sha256sum --check --ignore-missing "$CHECKSUMS" ); then
    printf 'error: SHA256 verification failed for %s\n' "$TARBALL" >&2
    printf '       expected (from %s):\n' "$CHECKSUMS" >&2
    grep -F "$TARBALL" "${TMP}/${CHECKSUMS}" >&2 || true
    printf '       observed:\n' >&2
    sha256sum "${TMP}/${TARBALL}" >&2 || true
    exit 1
fi

EXTRACT_DIR="${TMP}/extract"
mkdir -p "$EXTRACT_DIR"
tar -xzf "${TMP}/${TARBALL}" -C "$EXTRACT_DIR"

ROOT_DIR="${EXTRACT_DIR}/snip-${VERSION}-${PLATFORM}"
if [ ! -d "${ROOT_DIR}/bin" ]; then
    printf 'error: tarball layout unexpected (no bin/ under %s)\n' "${ROOT_DIR}" >&2
    exit 1
fi

for bin in sum-node e2e-helper; do
    SRC="${ROOT_DIR}/bin/${bin}"
    if [ ! -f "$SRC" ]; then
        printf 'error: missing binary in tarball: %s\n' "$bin" >&2
        exit 1
    fi
    DEST="${INSTALL_DIR}/${bin}"
    if [ -f "$DEST" ]; then
        PREV="$("$DEST" --version 2>/dev/null || true)"
        if [ -n "$PREV" ]; then
            printf 'snip-install: replacing existing %s (was: %s)\n' "$DEST" "$PREV"
        else
            printf 'snip-install: replacing existing %s\n' "$DEST"
        fi
    fi
    mv "$SRC" "$DEST"
    chmod +x "$DEST"
done

printf 'snip-install: installed sum-node   -> %s/sum-node\n'   "$INSTALL_DIR"
printf 'snip-install: installed e2e-helper -> %s/e2e-helper\n' "$INSTALL_DIR"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        cat <<EOF

note: ${INSTALL_DIR} is not on your \$PATH.
      Add this line to your shell rc (e.g. ~/.bashrc or ~/.zshrc):

          export PATH="${INSTALL_DIR}:\$PATH"

      Or invoke the binary by its full path:
          ${INSTALL_DIR}/sum-node --version
EOF
        ;;
esac

if [ -x "${INSTALL_DIR}/sum-node" ]; then
    REPORTED="$("${INSTALL_DIR}/sum-node" --version 2>/dev/null || echo '<unable to invoke>')"
    printf 'snip-install: sum-node reports: %s\n' "$REPORTED"
fi
