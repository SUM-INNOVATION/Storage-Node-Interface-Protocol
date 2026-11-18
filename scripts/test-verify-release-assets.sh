#!/usr/bin/env bash
# Self-test for scripts/verify-release-assets.sh against synthetic fixtures.
#
# Each case stages assets the way release.yml does (tar czf of
# snip-<tag>-linux-x86_64/ and snip-<tag>-linux-aarch64/, one sha256sum of
# both > SHA256SUMS, install.sh), with stub sum-node binaries that print a
# version, then breaks one property. The good case must pass; every other
# case must fail with its specific reason. The verifier runs only this
# host's platform binary (check 6), so the version cases break that one.
# No network, no real binaries, temp directories only. Run by
# `make release-check`.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
VERIFY="${HERE}/verify-release-assets.sh"
TAG="v9.8.7-rc6"
VERSION="${TAG#v}"
X86_ROOT="snip-${TAG}-linux-x86_64"
ARM_ROOT="snip-${TAG}-linux-aarch64"
# The platform whose binary the verifier runs on this host (same mapping).
case "$(uname -m)" in
    x86_64 | amd64) HOST_ROOT="$X86_ROOT"; OTHER_ROOT="$ARM_ROOT" ;;
    aarch64 | arm64) HOST_ROOT="$ARM_ROOT"; OTHER_ROOT="$X86_ROOT" ;;
    *) echo "test-verify-release-assets: unsupported host architecture $(uname -m)" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
printf '#!/bin/sh\necho "install stub"\n' > "$TMP/install.sh"

sha256_line() {
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && shift && sha256sum "$@")
    else
        (cd "$1" && shift && shasum -a 256 "$@")
    fi
}

# stage <case-dir> [<version printed by the host platform's sum-node>]: a
# correct release in <case-dir>/assets, built from <case-dir>/src, which the
# caller may edit and re-pack.
stage() {
    local d="$1" printed="${2:-$VERSION}" r p
    mkdir -p "$d/assets"
    for r in "$X86_ROOT" "$ARM_ROOT"; do
        p="$VERSION"
        if [ "$r" = "$HOST_ROOT" ]; then p="$printed"; fi
        mkdir -p "$d/src/$r/bin"
        printf '#!/bin/sh\necho "sum-node %s"\n' "$p" > "$d/src/$r/bin/sum-node"
        printf '#!/bin/sh\nexit 0\n' > "$d/src/$r/bin/e2e-helper"
        chmod 755 "$d/src/$r/bin/sum-node" "$d/src/$r/bin/e2e-helper"
        printf '# SNIP\n' > "$d/src/$r/README.md"
        printf '# Changelog\n\n## [%s] - TBD\n' "$VERSION" > "$d/src/$r/CHANGELOG.md"
        printf 'MIT\n' > "$d/src/$r/LICENSE-MIT"
        printf 'Apache-2.0\n' > "$d/src/$r/LICENSE-APACHE"
    done
    cp "$TMP/install.sh" "$d/assets/install.sh"
    (cd "$d/src" && tar czf "../assets/${X86_ROOT}.tar.gz" "$X86_ROOT")
    (cd "$d/src" && tar czf "../assets/${ARM_ROOT}.tar.gz" "$ARM_ROOT")
    sums "$d"
}
# pack <case-dir> <root> [<directory packed in its place>]: re-pack the
# tarball named after <root>, then SHA256SUMS over both tarballs.
pack() {
    local d="$1" r="$2" src="${3:-$2}"
    (cd "$d/src" && tar czf "../assets/${r}.tar.gz" "$src")
    sums "$d"
}
sums() {
    sha256_line "$1/assets" "${X86_ROOT}.tar.gz" "${ARM_ROOT}.tar.gz" > "$1/assets/SHA256SUMS"
}

pass=0
failed=0
# expect <name> <ok|fail> <stderr substring> <case-dir> [extra verifier args]
expect() {
    local name="$1" want="$2" needle="$3" d="$4"
    shift 4
    local rc=0
    "$VERIFY" "$d/assets" "$TAG" "$@" >"$d/out" 2>"$d/err" || rc=$?
    if [ "$want" = ok ]; then
        if [ "$rc" -eq 0 ] && grep -qF "$needle" "$d/out"; then
            pass=$((pass + 1)); echo "ok   $name"
            return
        fi
    elif [ "$rc" -eq 1 ] && grep -qF "$needle" "$d/err"; then
        pass=$((pass + 1)); echo "ok   $name"
        return
    fi
    failed=$((failed + 1))
    echo "FAIL $name (exit $rc; wanted $want with '$needle')"
    sed 's/^/     | /' "$d/out" "$d/err"
}

c="$TMP/good"; stage "$c"
expect "good release passes" ok "verify-release-assets: ok (${TAG}" "$c" "$TMP/install.sh"

c="$TMP/extra-asset"; stage "$c"; echo x > "$c/assets/notes.txt"
expect "extra asset refused" fail "asset set differs" "$c"

c="$TMP/missing-asset"; stage "$c"; rm "$c/assets/install.sh"
expect "missing asset refused" fail "asset set differs" "$c"

c="$TMP/x86-only"; stage "$c"; rm "$c/assets/${ARM_ROOT}.tar.gz"
sha256_line "$c/assets" "${X86_ROOT}.tar.gz" > "$c/assets/SHA256SUMS"
expect "release without the aarch64 tarball refused" fail "asset set differs" "$c"

c="$TMP/symlink-asset"; stage "$c"; rm "$c/assets/install.sh"; ln -s "$TMP/install.sh" "$c/assets/install.sh"
expect "symlinked asset refused" fail "asset is not a regular file: install.sh" "$c"

c="$TMP/bad-checksum"; stage "$c"
printf '%064d  %s\n' 0 "${X86_ROOT}.tar.gz" > "$c/assets/SHA256SUMS"
sha256_line "$c/assets" "${ARM_ROOT}.tar.gz" >> "$c/assets/SHA256SUMS"
expect "x86_64 checksum mismatch refused" fail "checksum mismatch for ${X86_ROOT}.tar.gz" "$c"

c="$TMP/tampered-tarball"; stage "$c"; cp "$c/assets/SHA256SUMS" "$c/sums"
echo tampered >> "$c/src/$ARM_ROOT/README.md"; pack "$c" "$ARM_ROOT"; cp "$c/sums" "$c/assets/SHA256SUMS"
expect "tampered aarch64 tarball refused" fail "checksum mismatch for ${ARM_ROOT}.tar.gz" "$c"

c="$TMP/three-sums"; stage "$c"; sha256_line "$c/assets" install.sh >> "$c/assets/SHA256SUMS"
expect "SHA256SUMS with a third entry refused" fail "exactly two entries" "$c"

c="$TMP/one-sum"; stage "$c"; sha256_line "$c/assets" "${X86_ROOT}.tar.gz" > "$c/assets/SHA256SUMS"
expect "SHA256SUMS with one entry refused" fail "exactly two entries" "$c"

c="$TMP/same-twice"; stage "$c"
sha256_line "$c/assets" "${X86_ROOT}.tar.gz" "${X86_ROOT}.tar.gz" > "$c/assets/SHA256SUMS"
expect "SHA256SUMS naming one tarball twice refused" fail "name each tarball once" "$c"

c="$TMP/sums-other-name"; stage "$c"
sed "s/  ${ARM_ROOT}\.tar\.gz\$/  snip-v0.0.1-linux-aarch64.tar.gz/" "$c/assets/SHA256SUMS" > "$c/s" && mv "$c/s" "$c/assets/SHA256SUMS"
expect "SHA256SUMS naming another file refused" fail "SHA256SUMS names" "$c"

c="$TMP/wrong-version"; stage "$c" "9.8.7-rc5"
expect "wrong --version refused" fail "printed 'sum-node 9.8.7-rc5'" "$c"

c="$TMP/version-fails"; stage "$c"
printf '#!/bin/sh\nexit 3\n' > "$c/src/$HOST_ROOT/bin/sum-node"; pack "$c" "$HOST_ROOT"
expect "failing --version refused" fail "bin/sum-node --version exited non-zero" "$c"

c="$TMP/wrong-root"; stage "$c"; mv "$c/src/$X86_ROOT" "$c/src/snip-other"; pack "$c" "$X86_ROOT" snip-other
expect "wrong root directory refused" fail "outside ${X86_ROOT}/" "$c"

c="$TMP/swapped-root"; stage "$c"; pack "$c" "$ARM_ROOT" "$X86_ROOT"
expect "aarch64 tarball holding the x86_64 tree refused" fail "outside ${ARM_ROOT}/" "$c"

c="$TMP/extra-file"; stage "$c"; echo x > "$c/src/$OTHER_ROOT/bin/extra"; pack "$c" "$OTHER_ROOT"
expect "extra file in the other platform's tarball refused" fail "${OTHER_ROOT}.tar.gz layout differs" "$c"

c="$TMP/missing-file"; stage "$c"; rm "$c/src/$ARM_ROOT/LICENSE-MIT"; pack "$c" "$ARM_ROOT"
expect "missing file in tarball refused" fail "${ARM_ROOT}.tar.gz layout differs" "$c"

c="$TMP/symlink-in-tarball"; stage "$c"; ln -s README.md "$c/src/$X86_ROOT/NOTES.md"; pack "$c" "$X86_ROOT"
expect "symlink in tarball refused" fail "link or special file" "$c"

c="$TMP/not-executable"; stage "$c"; chmod 644 "$c/src/$ARM_ROOT/bin/e2e-helper"; pack "$c" "$ARM_ROOT"
expect "non-executable binary refused" fail "${ARM_ROOT}.tar.gz: bin/e2e-helper is not executable" "$c"

c="$TMP/no-changelog-section"; stage "$c"
printf '# Changelog\n\n## [Unreleased]\n' > "$c/src/$OTHER_ROOT/CHANGELOG.md"; pack "$c" "$OTHER_ROOT"
expect "CHANGELOG without the version's section refused" fail "no '## [${VERSION}]' section" "$c"

c="$TMP/dotdot-entry"; stage "$c"
python3 - "$c/assets/${ARM_ROOT}.tar.gz" "$ARM_ROOT" <<'PY'
import io, sys, tarfile
path, root = sys.argv[1], sys.argv[2]
with tarfile.open(path, "w:gz") as t:
    data = b"x\n"
    info = tarfile.TarInfo(root + "/../escaped")
    info.size = len(data)
    t.addfile(info, io.BytesIO(data))
PY
sums "$c"
expect "'..' entry refused before unpacking" fail "'..' component" "$c"

c="$TMP/installer-differs"; stage "$c"; echo "# changed" >> "$c/assets/install.sh"
expect "install.sh differing from the repository's refused" fail "install.sh differs" "$c" "$TMP/install.sh"

total=$((pass + failed))
if [ "$failed" -ne 0 ]; then
    echo "test-verify-release-assets: ${failed} of ${total} cases FAILED" >&2
    exit 1
fi
echo "test-verify-release-assets: ok (${total} cases)"
