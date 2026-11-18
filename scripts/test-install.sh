#!/usr/bin/env bash
# Self-test for scripts/install.sh: architecture selection and checksum
# verification, against a synthetic release.
#
# The release is staged the way release.yml stages it: one tarball per
# platform (snip-<tag>-linux-x86_64.tar.gz, snip-<tag>-linux-aarch64.tar.gz),
# one SHA256SUMS over both. Each tarball's stub e2e-helper prints its
# platform, so a case can tell which tarball was installed. `uname` and
# `curl` are stubs on PATH: `uname -s` / `uname -m` report the case's
# values, and `curl` serves files from the staged release and logs each
# URL. Everything else (sh, tar, sha256sum, mktemp) is the host's own.
# No network, no real binaries, temp directories only. Run by
# `make release-check`.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
INSTALL="${HERE}/install.sh"
# The shell that runs the installer, as in the documented `| sh -s --`.
# Set INSTALL_TEST_SHELL to try another one (e.g. dash, busybox sh).
SH="${INSTALL_TEST_SHELL:-sh}"
TAG="v9.8.7-rc6"
VERSION="${TAG#v}"
BASE_URL="https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/${TAG}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

sha256_line() {
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && shift && sha256sum "$@")
    else
        (cd "$1" && shift && shasum -a 256 "$@")
    fi
}

# Stubs. They read their behaviour from the environment of each case.
mkdir -p "$TMP/stubbin"
cat > "$TMP/stubbin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' "$STUB_UNAME_S" ;;
    -m) printf '%s\n' "$STUB_UNAME_M" ;;
    *)  echo "uname stub: unexpected arguments: $*" >&2; exit 1 ;;
esac
EOF
cat > "$TMP/stubbin/curl" <<'EOF'
#!/bin/sh
# Serves "$STUB_RELEASE/<basename of URL>"; a missing file is an HTTP 404,
# which `curl -f` reports with exit 22.
out=""
url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        --proto) shift 2 ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
printf '%s\n' "$url" >> "$STUB_CURL_LOG"
src="$STUB_RELEASE/${url##*/}"
if [ ! -f "$src" ]; then
    echo "curl: (22) The requested URL returned error: 404" >&2
    exit 22
fi
cp "$src" "$out"
EOF
chmod 755 "$TMP/stubbin/uname" "$TMP/stubbin/curl"

# stage <release-dir>: a correct two-platform release in <release-dir>,
# built from <release-dir>.src, which the caller may edit and re-pack.
stage() {
    local d="$1" p root
    mkdir -p "$d" "$d.src"
    for p in linux-x86_64 linux-aarch64; do
        root="snip-${TAG}-${p}"
        mkdir -p "$d.src/$root/bin"
        printf '#!/bin/sh\necho "sum-node %s"\n' "$VERSION" > "$d.src/$root/bin/sum-node"
        printf '#!/bin/sh\necho "%s"\n' "$p" > "$d.src/$root/bin/e2e-helper"
        chmod 755 "$d.src/$root/bin/sum-node" "$d.src/$root/bin/e2e-helper"
        for f in README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE; do
            printf '%s\n' "$f" > "$d.src/$root/$f"
        done
        (cd "$d.src" && tar czf "$d/${root}.tar.gz" "$root")
    done
    sha256_line "$d" "snip-${TAG}-linux-x86_64.tar.gz" "snip-${TAG}-linux-aarch64.tar.gz" \
        > "$d/SHA256SUMS"
}

pass=0
failed=0
# run <case-dir> <uname -s> <uname -m> <release-dir>: run the installer
# with --prefix <case-dir>/prefix; sets $rc.
run() {
    local c="$1"
    mkdir -p "$c/home" "$c/prefix"
    : > "$c/curl.log"
    rc=0
    env HOME="$c/home" PATH="$TMP/stubbin:$PATH" \
        STUB_UNAME_S="$2" STUB_UNAME_M="$3" STUB_RELEASE="$4" STUB_CURL_LOG="$c/curl.log" \
        "$SH" "$INSTALL" --version "$TAG" --prefix "$c/prefix" >"$c/out" 2>"$c/err" || rc=$?
}
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() {
    failed=$((failed + 1))
    echo "FAIL $1: $2"
    sed 's/^/     | /' "$3/out" "$3/err" "$3/curl.log"
}

# expect_install <name> <uname -s> <uname -m> <platform>: the installer
# downloads SHA256SUMS and then <platform>'s tarball, nothing else, and
# installs that tarball's binaries.
expect_install() {
    local name="$1" s="$2" m="$3" want="$4" c
    c="$TMP/case-$((pass + failed))"
    run "$c" "$s" "$m" "$TMP/release"
    local urls
    urls="$(printf '%s\n' "${BASE_URL}/SHA256SUMS" "${BASE_URL}/snip-${TAG}-${want}.tar.gz")"
    if [ "$rc" -ne 0 ]; then
        bad "$name" "exit $rc, wanted 0" "$c"
    elif [ "$(cat "$c/curl.log")" != "$urls" ]; then
        bad "$name" "downloaded other URLs than SHA256SUMS and the ${want} tarball" "$c"
    elif [ "$("$c/prefix/bin/sum-node" --version)" != "sum-node ${VERSION}" ]; then
        bad "$name" "installed sum-node does not report ${VERSION}" "$c"
    elif [ "$("$c/prefix/bin/e2e-helper")" != "$want" ]; then
        bad "$name" "installed binaries are not the ${want} tarball's" "$c"
    else
        ok "$name"
    fi
}

# expect_refusal <name> <rc> <stderr substring> <uname -s> <uname -m>
#                <release-dir> <URLs downloaded, one per line, or "">:
# the installer exits <rc> with the message, downloads exactly those
# URLs, and leaves the pre-existing sum-node in the prefix untouched.
expect_refusal() {
    local name="$1" want_rc="$2" needle="$3" s="$4" m="$5" rel="$6" urls="$7" c
    c="$TMP/case-$((pass + failed))"
    mkdir -p "$c/prefix/bin"
    printf '#!/bin/sh\necho "sum-node 0.0.1"\n' > "$c/prefix/bin/sum-node"
    chmod 755 "$c/prefix/bin/sum-node"
    run "$c" "$s" "$m" "$rel"
    if [ "$rc" -ne "$want_rc" ]; then
        bad "$name" "exit $rc, wanted $want_rc" "$c"
    elif ! grep -qF -- "$needle" "$c/err"; then
        bad "$name" "stderr lacks '$needle'" "$c"
    elif [ "$(cat "$c/curl.log")" != "$urls" ]; then
        bad "$name" "downloaded URLs differ from the expected ones" "$c"
    elif [ "$("$c/prefix/bin/sum-node" --version)" != "sum-node 0.0.1" ] \
        || [ -e "$c/prefix/bin/e2e-helper" ]; then
        bad "$name" "the prefix was changed" "$c"
    else
        ok "$name"
    fi
}

stage "$TMP/release"

# Architecture selection.
expect_install "Linux x86_64 installs linux-x86_64" Linux x86_64 linux-x86_64
expect_install "Linux amd64 installs linux-x86_64" Linux amd64 linux-x86_64
expect_install "Linux aarch64 installs linux-aarch64" Linux aarch64 linux-aarch64
expect_install "Linux arm64 installs linux-aarch64" Linux arm64 linux-aarch64

# Unsupported platforms are refused before any download.
for m in armv7l riscv64 i686 ppc64le s390x; do
    expect_refusal "Linux ${m} refused" 2 "detected: Linux ${m}" Linux "$m" "$TMP/release" ""
done
expect_refusal "macOS arm64 refused" 2 "detected: Darwin arm64" Darwin arm64 "$TMP/release" ""

sums_url="${BASE_URL}/SHA256SUMS"
x86_url="${BASE_URL}/snip-${TAG}-linux-x86_64.tar.gz"
arm_url="${BASE_URL}/snip-${TAG}-linux-aarch64.tar.gz"

# Checksum mismatches are refused before anything is extracted or installed.
r="$TMP/rel-bad-digest"; stage "$r"
sed "s/^[0-9a-f]*  \(snip-${TAG}-linux-aarch64\.tar\.gz\)\$/$(printf '%064d' 0)  \1/" \
    "$r/SHA256SUMS" > "$r/s" && mv "$r/s" "$r/SHA256SUMS"
expect_refusal "aarch64: SHA256SUMS digest mismatch refused" 1 "SHA256 verification failed" \
    Linux aarch64 "$r" "$(printf '%s\n' "$sums_url" "$arm_url")"

r="$TMP/rel-tampered-x86"; stage "$r"
echo tampered >> "$r.src/snip-${TAG}-linux-x86_64/README.md"
(cd "$r.src" && tar czf "$r/snip-${TAG}-linux-x86_64.tar.gz" "snip-${TAG}-linux-x86_64")
expect_refusal "x86_64: tampered tarball refused" 1 "SHA256 verification failed" \
    Linux x86_64 "$r" "$(printf '%s\n' "$sums_url" "$x86_url")"

r="$TMP/rel-swapped"; stage "$r"
sed -e "s/linux-x86_64/linux-SWAP/" -e "s/linux-aarch64/linux-x86_64/" -e "s/linux-SWAP/linux-aarch64/" \
    "$r/SHA256SUMS" > "$r/s" && mv "$r/s" "$r/SHA256SUMS"
expect_refusal "aarch64: the x86_64 digest is not accepted" 1 "SHA256 verification failed" \
    Linux aarch64 "$r" "$(printf '%s\n' "$sums_url" "$arm_url")"

# A release without this platform's tarball (any release cut before the
# aarch64 tarball was added) is refused before the tarball is fetched.
r="$TMP/rel-x86-only"; stage "$r"
rm "$r/snip-${TAG}-linux-aarch64.tar.gz"
grep -F "linux-x86_64" "$r/SHA256SUMS" > "$r/s" && mv "$r/s" "$r/SHA256SUMS"
expect_refusal "aarch64: release without an aarch64 tarball refused" 1 \
    "does not list snip-${TAG}-linux-aarch64.tar.gz" Linux aarch64 "$r" "$sums_url"

r="$TMP/rel-twice"; stage "$r"
grep -F "linux-aarch64" "$r/SHA256SUMS" > "$r/s" && cat "$r/s" >> "$r/SHA256SUMS"
expect_refusal "aarch64: tarball listed twice refused" 1 "lists snip-${TAG}-linux-aarch64.tar.gz 2 times" \
    Linux aarch64 "$r" "$sums_url"

total=$((pass + failed))
if [ "$failed" -ne 0 ]; then
    echo "test-install: ${failed} of ${total} cases FAILED" >&2
    exit 1
fi
echo "test-install: ok (${total} cases)"
