#!/usr/bin/env bash
# Self-test for scripts/verify-release-assets.sh against synthetic fixtures.
#
# Each case stages assets the way release.yml does (tar czf of
# snip-<tag>-linux-x86_64/, sha256sum > SHA256SUMS, install.sh), with a
# stub sum-node that prints a version, then breaks one property. The good
# case must pass; every other case must fail with its specific reason.
# No network, no real binaries, temp directories only. Run by
# `make release-check`.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
VERIFY="${HERE}/verify-release-assets.sh"
TAG="v9.8.7-rc6"
VERSION="${TAG#v}"
ROOT="snip-${TAG}-linux-x86_64"
TARBALL="${ROOT}.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
printf '#!/bin/sh\necho "install stub"\n' > "$TMP/install.sh"

sha256_line() {
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && sha256sum "$2")
    else
        (cd "$1" && shasum -a 256 "$2")
    fi
}

# stage <case-dir> [<printed version>]: a correct release in <case-dir>/assets,
# built from <case-dir>/src, which the caller may edit and re-pack.
stage() {
    local d="$1" printed="${2:-$VERSION}"
    mkdir -p "$d/src/$ROOT/bin" "$d/assets"
    printf '#!/bin/sh\necho "sum-node %s"\n' "$printed" > "$d/src/$ROOT/bin/sum-node"
    printf '#!/bin/sh\nexit 0\n' > "$d/src/$ROOT/bin/e2e-helper"
    chmod 755 "$d/src/$ROOT/bin/sum-node" "$d/src/$ROOT/bin/e2e-helper"
    printf '# SNIP\n' > "$d/src/$ROOT/README.md"
    printf '# Changelog\n\n## [%s] - TBD\n' "$VERSION" > "$d/src/$ROOT/CHANGELOG.md"
    printf 'MIT\n' > "$d/src/$ROOT/LICENSE-MIT"
    printf 'Apache-2.0\n' > "$d/src/$ROOT/LICENSE-APACHE"
    cp "$TMP/install.sh" "$d/assets/install.sh"
    pack "$d"
}
# pack <case-dir> [<root-name>]: re-pack the tarball and its checksum.
pack() {
    local d="$1" r="${2:-$ROOT}"
    (cd "$d/src" && tar czf "../assets/$TARBALL" "$r")
    sha256_line "$d/assets" "$TARBALL" > "$d/assets/SHA256SUMS"
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

c="$TMP/symlink-asset"; stage "$c"; rm "$c/assets/install.sh"; ln -s "$TMP/install.sh" "$c/assets/install.sh"
expect "symlinked asset refused" fail "asset is not a regular file: install.sh" "$c"

c="$TMP/bad-checksum"; stage "$c"
printf '%064d  %s\n' 0 "$TARBALL" > "$c/assets/SHA256SUMS"
expect "checksum mismatch refused" fail "checksum mismatch" "$c"

c="$TMP/tampered-tarball"; stage "$c"; sums="$(cat "$c/assets/SHA256SUMS")"
echo tampered >> "$c/src/$ROOT/README.md"; pack "$c"; echo "$sums" > "$c/assets/SHA256SUMS"
expect "tampered tarball refused" fail "checksum mismatch" "$c"

c="$TMP/two-sums"; stage "$c"; sha256_line "$c/assets" install.sh >> "$c/assets/SHA256SUMS"
expect "SHA256SUMS with a second entry refused" fail "exactly one entry" "$c"

c="$TMP/sums-other-name"; stage "$c"
sed "s/  .*/  snip-v0.0.1-linux-x86_64.tar.gz/" "$c/assets/SHA256SUMS" > "$c/s" && mv "$c/s" "$c/assets/SHA256SUMS"
expect "SHA256SUMS naming another file refused" fail "SHA256SUMS names" "$c"

c="$TMP/wrong-version"; stage "$c" "9.8.7-rc5"
expect "wrong --version refused" fail "printed 'sum-node 9.8.7-rc5'" "$c"

c="$TMP/wrong-root"; stage "$c"; mv "$c/src/$ROOT" "$c/src/snip-other"; pack "$c" snip-other
expect "wrong root directory refused" fail "outside ${ROOT}/" "$c"

c="$TMP/extra-file"; stage "$c"; echo x > "$c/src/$ROOT/bin/extra"; pack "$c"
expect "extra file in tarball refused" fail "tarball layout differs" "$c"

c="$TMP/missing-file"; stage "$c"; rm "$c/src/$ROOT/LICENSE-MIT"; pack "$c"
expect "missing file in tarball refused" fail "tarball layout differs" "$c"

c="$TMP/symlink-in-tarball"; stage "$c"; ln -s README.md "$c/src/$ROOT/NOTES.md"; pack "$c"
expect "symlink in tarball refused" fail "link or special file" "$c"

c="$TMP/not-executable"; stage "$c"; chmod 644 "$c/src/$ROOT/bin/e2e-helper"; pack "$c"
expect "non-executable binary refused" fail "bin/e2e-helper is not executable" "$c"

c="$TMP/no-changelog-section"; stage "$c"; printf '# Changelog\n\n## [Unreleased]\n' > "$c/src/$ROOT/CHANGELOG.md"; pack "$c"
expect "CHANGELOG without the version's section refused" fail "no '## [${VERSION}]' section" "$c"

c="$TMP/dotdot-entry"; stage "$c"
python3 - "$c/assets/$TARBALL" "$ROOT" <<'PY'
import io, sys, tarfile
path, root = sys.argv[1], sys.argv[2]
with tarfile.open(path, "w:gz") as t:
    data = b"x\n"
    info = tarfile.TarInfo(root + "/../escaped")
    info.size = len(data)
    t.addfile(info, io.BytesIO(data))
PY
sha256_line "$c/assets" "$TARBALL" > "$c/assets/SHA256SUMS"
expect "'..' entry refused before unpacking" fail "'..' component" "$c"

c="$TMP/installer-differs"; stage "$c"; echo "# changed" >> "$c/assets/install.sh"
expect "install.sh differing from the repository's refused" fail "install.sh differs" "$c" "$TMP/install.sh"

total=$((pass + failed))
if [ "$failed" -ne 0 ]; then
    echo "test-verify-release-assets: ${failed} of ${total} cases FAILED" >&2
    exit 1
fi
echo "test-verify-release-assets: ok (${total} cases)"
