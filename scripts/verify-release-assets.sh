#!/usr/bin/env bash
# Verify a SNIP release's assets, as downloaded from its GitHub Release.
#
# Usage:
#   scripts/verify-release-assets.sh <asset-dir> <tag> [<expected-install.sh>]
#
# <asset-dir> must hold exactly what the release carries, nothing else:
#   snip-<tag>-linux-x86_64.tar.gz, SHA256SUMS, install.sh
#
# Checks, each fail-closed (exit 1 with a "FAIL:" line on stderr):
#   1. the asset set is exactly those three regular files;
#   2. SHA256SUMS has exactly one entry, for the tarball, and it matches;
#   3. every tarball entry lives under snip-<tag>-linux-x86_64/, with no
#      absolute path, no ".." component, and no link or special file;
#   4. the unpacked tree holds exactly bin/sum-node, bin/e2e-helper,
#      README.md, CHANGELOG.md, LICENSE-MIT and LICENSE-APACHE, with both
#      binaries executable;
#   5. the bundled CHANGELOG.md has a "## [<version>]" section;
#   6. `bin/sum-node --version` prints exactly "sum-node <version>";
#   7. if <expected-install.sh> is given, the asset install.sh is
#      byte-identical to it.
# <version> is <tag> without its leading "v". Usage errors exit 2.
#
# release.yml runs this on fresh GitHub-hosted runners against the draft;
# scripts/test-verify-release-assets.sh self-tests it in `make release-check`.
# Runs on Linux and macOS (bash 3.2+, sha256sum or shasum).

set -euo pipefail

usage() {
    echo "usage: $0 <asset-dir> <tag> [<expected-install.sh>]" >&2
    exit 2
}
fail() {
    echo "verify-release-assets: FAIL: $*" >&2
    exit 1
}

[ $# -eq 2 ] || [ $# -eq 3 ] || usage
ASSETS="$1"
TAG="$2"
EXPECT_INSTALL="${3:-}"

case "$TAG" in
    v[0-9]*.[0-9]*.[0-9]*-rc[0-9]*) ;;
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "error: tag '$TAG' does not match vX.Y.Z or vX.Y.Z-rcN" >&2; exit 2 ;;
esac
[ -d "$ASSETS" ] || { echo "error: not a directory: $ASSETS" >&2; exit 2; }
if [ -n "$EXPECT_INSTALL" ] && [ ! -f "$EXPECT_INSTALL" ]; then
    echo "error: expected install.sh not found: $EXPECT_INSTALL" >&2
    exit 2
fi

VERSION="${TAG#v}"
ROOT="snip-${TAG}-linux-x86_64"
TARBALL="${ROOT}.tar.gz"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# 1. Exactly the three expected assets, each a regular file.
expected_assets="$(printf '%s\n' SHA256SUMS install.sh "$TARBALL" | LC_ALL=C sort)"
actual_assets="$(cd "$ASSETS" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||' | LC_ALL=C sort)"
if [ "$actual_assets" != "$expected_assets" ]; then
    fail "asset set differs from {${TARBALL}, SHA256SUMS, install.sh}; found: $(echo "$actual_assets" | tr '\n' ' ')"
fi
for a in SHA256SUMS install.sh "$TARBALL"; do
    if [ -L "$ASSETS/$a" ] || [ ! -f "$ASSETS/$a" ]; then
        fail "asset is not a regular file: $a"
    fi
done

# 2. SHA256SUMS names exactly the tarball, and the digest matches.
lines="$(grep -c '' "$ASSETS/SHA256SUMS" || true)"
[ "$lines" = "1" ] || fail "SHA256SUMS must have exactly one entry (has ${lines} lines)"
line="$(cat "$ASSETS/SHA256SUMS")"
want="${line%%  *}"
name="${line#*  }"
case "$want" in
    *[!0-9a-f]* | "") fail "SHA256SUMS entry is not '<sha256>  <file>': ${line}" ;;
esac
[ "${#want}" -eq 64 ] || fail "SHA256SUMS digest is not 64 hex characters"
[ "$name" = "$TARBALL" ] || fail "SHA256SUMS names '${name}', expected '${TARBALL}'"
got="$(sha256_of "$ASSETS/$TARBALL")"
[ "$got" = "$want" ] || fail "checksum mismatch for ${TARBALL}: SHA256SUMS ${want}, actual ${got}"

# 3. Every entry is confined to the expected root before anything is unpacked.
entries="$(tar -tzf "$ASSETS/$TARBALL")" || fail "cannot list ${TARBALL}"
[ -n "$entries" ] || fail "${TARBALL} is empty"
while IFS= read -r e; do
    case "$e" in
        "$ROOT" | "$ROOT/" | "$ROOT"/*) ;;
        *) fail "tarball entry outside ${ROOT}/: ${e}" ;;
    esac
    case "/$e/" in
        */../*) fail "tarball entry has a '..' component: ${e}" ;;
    esac
done <<EOF
$entries
EOF

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
tar -xzf "$ASSETS/$TARBALL" -C "$WORK" || fail "cannot unpack ${TARBALL}"

top="$(cd "$WORK" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||')"
[ "$top" = "$ROOT" ] || fail "tarball top level is '$(echo "$top" | tr '\n' ' ')', expected '${ROOT}'"
special="$(cd "$WORK" && find "$ROOT" ! -type f ! -type d)"
[ -z "$special" ] || fail "tarball holds a link or special file: $(echo "$special" | tr '\n' ' ')"

# 4. Exact layout.
expected_files="$(printf '%s\n' bin/sum-node bin/e2e-helper README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE | LC_ALL=C sort)"
actual_files="$(cd "$WORK/$ROOT" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)"
if [ "$actual_files" != "$expected_files" ]; then
    fail "tarball layout differs; found: $(echo "$actual_files" | tr '\n' ' ')"
fi
for b in sum-node e2e-helper; do
    [ -x "$WORK/$ROOT/bin/$b" ] || fail "bin/${b} is not executable"
done

# 5. The bundled CHANGELOG carries this version's section.
grep -qF "## [${VERSION}]" "$WORK/$ROOT/CHANGELOG.md" \
    || fail "bundled CHANGELOG.md has no '## [${VERSION}]' section"

# 6. The binary runs here and reports the tag's version.
out="$("$WORK/$ROOT/bin/sum-node" --version 2>&1)" || fail "bin/sum-node --version exited non-zero: ${out}"
[ "$out" = "sum-node ${VERSION}" ] || fail "bin/sum-node --version printed '${out}', expected 'sum-node ${VERSION}'"

# 7. The published installer is the repository's.
if [ -n "$EXPECT_INSTALL" ]; then
    cmp -s "$ASSETS/install.sh" "$EXPECT_INSTALL" \
        || fail "install.sh differs from ${EXPECT_INSTALL}"
fi

echo "verify-release-assets: ok (${TAG}: ${TARBALL} sha256 ${got}; $(uname -s) $(uname -m))"
