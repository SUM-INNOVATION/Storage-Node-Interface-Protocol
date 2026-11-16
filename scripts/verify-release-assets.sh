#!/usr/bin/env bash
# Verify a SNIP release's assets, as downloaded from its GitHub Release.
#
# Usage:
#   scripts/verify-release-assets.sh <asset-dir> <tag> [<expected-install.sh>]
#
# <asset-dir> must hold exactly what the release carries, nothing else:
#   snip-<tag>-linux-x86_64.tar.gz, snip-<tag>-linux-aarch64.tar.gz,
#   SHA256SUMS, install.sh
#
# Checks, each fail-closed (exit 1 with a "FAIL:" line on stderr):
#   1. the asset set is exactly those four regular files;
#   2. SHA256SUMS has exactly two entries, one for each tarball, and both
#      match;
#   for each tarball, snip-<tag>-<platform>.tar.gz:
#   3. every entry lives under snip-<tag>-<platform>/, with no absolute
#      path, no ".." component, and no link or special file;
#   4. the unpacked tree holds exactly bin/sum-node, bin/e2e-helper,
#      README.md, CHANGELOG.md, LICENSE-MIT and LICENSE-APACHE, with both
#      binaries executable;
#   5. the bundled CHANGELOG.md has a "## [<version>]" section;
#   then:
#   6. `bin/sum-node --version` from the tarball for this host's
#      architecture (`uname -m`: x86_64/amd64 -> linux-x86_64,
#      aarch64/arm64 -> linux-aarch64, as in install.sh) prints exactly
#      "sum-node <version>"; on any other architecture this check fails;
#   7. if <expected-install.sh> is given, the asset install.sh is
#      byte-identical to it.
# <version> is <tag> without its leading "v". Usage errors exit 2.
#
# Check 6 runs one platform's binary per host, so release.yml runs this on
# fresh GitHub-hosted x86_64 and arm64 runners against the draft.
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
PLATFORMS="linux-x86_64 linux-aarch64"
X86_TARBALL="snip-${TAG}-linux-x86_64.tar.gz"
ARM_TARBALL="snip-${TAG}-linux-aarch64.tar.gz"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# 1. Exactly the four expected assets, each a regular file.
expected_assets="$(printf '%s\n' SHA256SUMS install.sh "$X86_TARBALL" "$ARM_TARBALL" | LC_ALL=C sort)"
actual_assets="$(cd "$ASSETS" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||' | LC_ALL=C sort)"
if [ "$actual_assets" != "$expected_assets" ]; then
    fail "asset set differs from {${X86_TARBALL}, ${ARM_TARBALL}, SHA256SUMS, install.sh}; found: $(echo "$actual_assets" | tr '\n' ' ')"
fi
for a in SHA256SUMS install.sh "$X86_TARBALL" "$ARM_TARBALL"; do
    if [ -L "$ASSETS/$a" ] || [ ! -f "$ASSETS/$a" ]; then
        fail "asset is not a regular file: $a"
    fi
done

# 2. SHA256SUMS names exactly the two tarballs, once each, and both digests
#    match.
lines="$(grep -c '' "$ASSETS/SHA256SUMS" || true)"
[ "$lines" = "2" ] || fail "SHA256SUMS must have exactly two entries (has ${lines} lines)"
names=""
digests=""
while IFS= read -r line; do
    want="${line%%  *}"
    name="${line#*  }"
    case "$want" in
        *[!0-9a-f]* | "") fail "SHA256SUMS entry is not '<sha256>  <file>': ${line}" ;;
    esac
    [ "${#want}" -eq 64 ] || fail "SHA256SUMS digest is not 64 hex characters"
    case "$name" in
        "$X86_TARBALL" | "$ARM_TARBALL") ;;
        *) fail "SHA256SUMS names '${name}', expected '${X86_TARBALL}' and '${ARM_TARBALL}'" ;;
    esac
    got="$(sha256_of "$ASSETS/$name")"
    [ "$got" = "$want" ] || fail "checksum mismatch for ${name}: SHA256SUMS ${want}, actual ${got}"
    names="${names}${name}"$'\n'
    digests="${digests}${name} sha256 ${got}; "
done < "$ASSETS/SHA256SUMS"
[ "$(printf '%s' "$names" | LC_ALL=C sort -u | grep -c '')" = "2" ] \
    || fail "SHA256SUMS must name each tarball once; it names: $(printf '%s' "$names" | tr '\n' ' ')"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for platform in $PLATFORMS; do
    ROOT="snip-${TAG}-${platform}"
    TARBALL="${ROOT}.tar.gz"

    # 3. Every entry is confined to the expected root before anything is
    #    unpacked.
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

    mkdir "$WORK/$platform"
    tar -xzf "$ASSETS/$TARBALL" -C "$WORK/$platform" || fail "cannot unpack ${TARBALL}"

    top="$(cd "$WORK/$platform" && find . -mindepth 1 -maxdepth 1 | sed 's|^\./||')"
    [ "$top" = "$ROOT" ] || fail "${TARBALL} top level is '$(echo "$top" | tr '\n' ' ')', expected '${ROOT}'"
    special="$(cd "$WORK/$platform" && find "$ROOT" ! -type f ! -type d)"
    [ -z "$special" ] || fail "${TARBALL} holds a link or special file: $(echo "$special" | tr '\n' ' ')"

    # 4. Exact layout.
    expected_files="$(printf '%s\n' bin/sum-node bin/e2e-helper README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE | LC_ALL=C sort)"
    actual_files="$(cd "$WORK/$platform/$ROOT" && find . -type f | sed 's|^\./||' | LC_ALL=C sort)"
    if [ "$actual_files" != "$expected_files" ]; then
        fail "${TARBALL} layout differs; found: $(echo "$actual_files" | tr '\n' ' ')"
    fi
    for b in sum-node e2e-helper; do
        [ -x "$WORK/$platform/$ROOT/bin/$b" ] || fail "${TARBALL}: bin/${b} is not executable"
    done

    # 5. The bundled CHANGELOG carries this version's section.
    grep -qF "## [${VERSION}]" "$WORK/$platform/$ROOT/CHANGELOG.md" \
        || fail "${TARBALL}: bundled CHANGELOG.md has no '## [${VERSION}]' section"
done

# 6. This host's platform binary runs here and reports the tag's version.
host_arch="$(uname -m)"
case "$host_arch" in
    x86_64 | amd64) host_platform="linux-x86_64" ;;
    aarch64 | arm64) host_platform="linux-aarch64" ;;
    *) fail "no tarball for this host's architecture (${host_arch}); run on x86_64 or aarch64" ;;
esac
ROOT="snip-${TAG}-${host_platform}"
out="$("$WORK/$host_platform/$ROOT/bin/sum-node" --version 2>&1)" \
    || fail "${ROOT}.tar.gz: bin/sum-node --version exited non-zero: ${out}"
[ "$out" = "sum-node ${VERSION}" ] \
    || fail "${ROOT}.tar.gz: bin/sum-node --version printed '${out}', expected 'sum-node ${VERSION}'"

# 7. The published installer is the repository's.
if [ -n "$EXPECT_INSTALL" ]; then
    cmp -s "$ASSETS/install.sh" "$EXPECT_INSTALL" \
        || fail "install.sh differs from ${EXPECT_INSTALL}"
fi

echo "verify-release-assets: ok (${TAG}: ${digests}ran ${host_platform} bin/sum-node on $(uname -s) ${host_arch})"
