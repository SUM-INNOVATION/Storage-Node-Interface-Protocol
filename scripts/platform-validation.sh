#!/usr/bin/env bash
# platform-validation.sh — the read-only cycle of the SNIP cross-platform
# validation instructions (issue #24), with every expected output checked.
#
# Usage (from the repository root, after the platform's pre-flight setup):
#   scripts/platform-validation.sh client [--rpc-url URL] [--no-smoke]
#   scripts/platform-validation.sh windows-fail-closed
#
# client
#   macOS (#24 § 2.2 + § 2.3), WSL2 (§ 3.4) and Crostini (§ 4.4). Records
#   the § 1 provenance, then runs exactly the documented commands:
#     cargo build --release -p sum-node
#     cargo test -p sum-node --bin e2e-helper
#     <target>/release/e2e-helper smoke --rpc-url <URL> --require-v2
#   and checks the outputs § 2.2 and § 2.3 list. The smoke reads two RPC
#   methods and writes nothing. --no-smoke stops after the tests and the
#   result is then PARTIAL, never PASS. SNIP_SMOKE=0 is the same as
#   --no-smoke; SNIP_RPC_URL is the same as --rpc-url.
#
# windows-fail-closed
#   Native Windows is not a SNIP target (SNIP #49). Runs
#   crates/sum-store/tests/platform_fail_closed.rs and requires exactly
#   its three tests to pass: store publication and the store lease refuse
#   with their unsupported-platform errors before creating anything.
#
# Never run by this script: client writes (§ 2.4 / 3.6 / 4.6), which need
# coordinator approval and funded keys, and archive mode (§ 5). The script
# reads no keys or seeds and needs none.
#
# Expected chain values default to mainnet; override them for another chain:
#   SNIP_EXPECT_CHAIN_ID (1)  SNIP_EXPECT_R (3)  SNIP_EXPECT_V2_FROM (5200000)
# Optional report fields: SNIP_PLATFORM_LABEL, SNIP_CELL_NOTE.
#
# Prints a § 7 report on stdout (also appended to $GITHUB_STEP_SUMMARY when
# set). Exit 0 = PASS or PARTIAL (no check failed), 1 = a check failed,
# 2 = usage error or wrong platform for the mode.

set -uo pipefail

RPC_URL="${SNIP_RPC_URL:-https://rpc.sumchain.io}"
EXPECT_CHAIN_ID="${SNIP_EXPECT_CHAIN_ID:-1}"
EXPECT_R="${SNIP_EXPECT_R:-3}"
EXPECT_V2_FROM="${SNIP_EXPECT_V2_FROM:-5200000}"
FAIL_CLOSED_TESTS=3
export CARGO_TERM_COLOR=never

usage() {
    sed -n '5,7p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

[ $# -ge 1 ] || usage
MODE=$1
shift
SMOKE="${SNIP_SMOKE:-1}"
while [ $# -gt 0 ]; do
    case "$1" in
        --rpc-url)
            [ $# -ge 2 ] || usage
            RPC_URL=$2
            shift 2
            ;;
        --no-smoke)
            SMOKE=0
            shift
            ;;
        *) usage ;;
    esac
done

LOGS=$(mktemp -d "${TMPDIR:-/tmp}/snip-platform.XXXXXX") || exit 1
trap 'rm -rf "${LOGS:?}"' EXIT

FAILED=0
RESULTS=""
COMMANDS=""
FAILURES=""

result() { RESULTS="${RESULTS}$1"$'\n'; }
ran() { COMMANDS="${COMMANDS}$*"$'\n'; }
fail() {
    FAILED=1
    FAILURES="${FAILURES}$1"$'\n'
    [ -n "${2:-}" ] && [ -s "$2" ] && FAILURES="${FAILURES}$(tail -n 20 "$2")"$'\n\n'
}

# Run a command, streaming stdout+stderr to stderr and keeping them in a log.
# Sets RC to the command's exit status. run_logged also records the command
# for the report.
run_logged() {
    ran "${*:2}"
    exec_logged "$@"
}

exec_logged() {
    local log=$1
    shift
    echo "+ $*" >&2
    "$@" 2>&1 | tee "$log" >&2
    RC=${PIPESTATUS[0]}
}

is_windows_native() {
    case "$(uname -s)" in
        MINGW* | MSYS* | CYGWIN* | Windows_NT) return 0 ;;
    esac
    [ "${OS:-}" = Windows_NT ]
}

detect_platform() {
    if [ -n "${SNIP_PLATFORM_LABEL:-}" ]; then
        echo "$SNIP_PLATFORM_LABEL"
    elif is_windows_native; then
        echo "Windows native"
    elif [ "$(uname -s)" = Darwin ]; then
        echo "macOS"
    elif grep -qi microsoft /proc/version 2>/dev/null; then
        echo "WSL2"
    elif [ -e /dev/.cros_milestone ]; then
        echo "Crostini"
    else
        echo "Linux"
    fi
}

os_version() {
    if is_windows_native; then
        cmd //c ver 2>/dev/null | tr -d '\r' | grep . || uname -sr
    elif [ "$(uname -s)" = Darwin ]; then
        sw_vers 2>/dev/null
    elif command -v lsb_release >/dev/null 2>&1; then
        lsb_release -a 2>/dev/null
    elif [ -r /etc/os-release ]; then
        grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release
    else
        uname -sr
    fi
}

# § 1: SNIP pins a minimum Rust (workspace rust-version).
check_toolchain() {
    local need have
    need=$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' Cargo.toml | head -n 1)
    have=$(rustc --version 2>/dev/null | sed -n 's/^rustc \([0-9]*\.[0-9]*\).*/\1/p')
    if [ -z "$have" ]; then
        fail "toolchain: rustc --version failed; fix the toolchain first (§ 1)"
        return 1
    fi
    if [ -n "$need" ] && ! version_at_least "$have" "$need"; then
        fail "toolchain: rustc $have is older than the workspace rust-version $need"
        return 1
    fi
    result "Toolchain: rustc $have >= workspace rust-version ${need:-?}: pass"
}

version_at_least() {
    local a_major=${1%%.*} a_minor=${1#*.} b_major=${2%%.*} b_minor=${2#*.}
    b_minor=${b_minor%%.*}
    [ "$a_major" -gt "$b_major" ] || { [ "$a_major" -eq "$b_major" ] && [ "$a_minor" -ge "$b_minor" ]; }
}

target_dir() {
    local dir
    dir=$(cargo metadata --format-version 1 --no-deps 2>/dev/null |
        sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
    echo "${dir:-target}"
}

# § 2.2: build, then the e2e-helper unit tests.
build_and_test() {
    run_logged "$LOGS/build.log" cargo build --release -p sum-node
    # shellcheck disable=SC2016  # literal backticks in cargo's output
    if [ "$RC" -eq 0 ] && grep -q 'Finished `release` profile' "$LOGS/build.log"; then
        result "Build (cargo build --release -p sum-node): pass"
    else
        result "Build (cargo build --release -p sum-node): FAIL (exit $RC)"
        fail "build: expected exit 0 and 'Finished \`release\` profile'" "$LOGS/build.log"
        return 1
    fi

    run_logged "$LOGS/test.log" cargo test -p sum-node --bin e2e-helper
    local summary
    summary=$(grep -E '^test result: ok\. [0-9]+ passed; 0 failed;' "$LOGS/test.log" | tail -n 1)
    if [ "$RC" -eq 0 ] && [ -n "$summary" ]; then
        result "e2e-helper unit tests: pass ($summary)"
    else
        result "e2e-helper unit tests: FAIL (exit $RC)"
        fail "e2e-helper tests: expected 'test result: ok. <N> passed; 0 failed'" "$LOGS/test.log"
        return 1
    fi
}

# § 2.3: read-only smoke. Each line below is the exact expected text.
smoke() {
    local bin shown log=$LOGS/smoke.log height dots=...........
    bin="$(target_dir)/release/e2e-helper"
    # Report the documented form, not an absolute path under a home directory.
    case "$bin" in
        "$PWD"/*) shown="./${bin#"$PWD"/}" ;;
        /*) shown='<target-dir>/release/e2e-helper' ;;
        *) shown="./$bin" ;;
    esac
    ran "$shown smoke --rpc-url $RPC_URL --require-v2"
    exec_logged "$log" "$bin" smoke --rpc-url "$RPC_URL" --require-v2
    SMOKE_OUTPUT=$(cat "$log")

    local expect_params="[1/2] chain_getChainParams $dots OK (chain_id=$EXPECT_CHAIN_ID, R=$EXPECT_R, v2_enabled_from_height=Some($EXPECT_V2_FROM))"
    local expect_v2="V2 state: ENABLED_FROM_HEIGHT (v2_enabled_from_height=Some($EXPECT_V2_FROM) → enabled at height $EXPECT_V2_FROM)"
    local missing=""
    for line in "smoke target: $RPC_URL" "$expect_params" "$expect_v2" "smoke: ok"; do
        grep -Fxq -- "$line" "$log" || missing="${missing}  expected line: $line"$'\n'
    done
    height=$(sed -n 's/^\[2\/2\] chain_getBlockHeight \.\{11\} OK (finalized height=\([0-9]*\), finality=finalized)$/\1/p' "$log")
    if [ -z "$height" ]; then
        missing="${missing}  expected line: [2/2] chain_getBlockHeight $dots OK (finalized height=<N>, finality=finalized)"$'\n'
    elif [ "$height" -lt "$EXPECT_V2_FROM" ]; then
        missing="${missing}  finalized height $height is below the V2 activation height $EXPECT_V2_FROM"$'\n'
    fi
    if [ "$(grep -c '^\[[0-9]*/2\] ' "$log")" -ne 2 ]; then
        missing="${missing}  expected exactly two checks ([1/2], [2/2]); opt-in checks are not part of § 2.3"$'\n'
    fi

    if [ "$RC" -eq 0 ] && [ -z "$missing" ]; then
        result "Smoke (e2e-helper smoke --require-v2): pass (finalized height=$height)"
    else
        result "Smoke (e2e-helper smoke --require-v2): FAIL (exit $RC)"
        # The full smoke output is already in the report.
        fail "smoke: exit $RC (expected 0)"$'\n'"$missing"
    fi
}

windows_fail_closed() {
    run_logged "$LOGS/fail-closed.log" cargo test -p sum-store --test platform_fail_closed
    local summary
    summary=$(grep -E "^test result: ok\. $FAIL_CLOSED_TESTS passed; 0 failed;" "$LOGS/fail-closed.log" | tail -n 1)
    if [ "$RC" -eq 0 ] && [ -n "$summary" ]; then
        result "Fail-closed (sum-store platform_fail_closed): pass ($summary)"
    else
        result "Fail-closed (sum-store platform_fail_closed): FAIL (exit $RC)"
        fail "fail-closed: expected 'test result: ok. $FAIL_CLOSED_TESTS passed; 0 failed'" "$LOGS/fail-closed.log"
    fi
}

report() {
    local verdict=$1 scope=$2
    cat <<EOF
=== SNIP cross-platform validation report ===

Platform: $(detect_platform)
${SNIP_CELL_NOTE:+Cell note: $SNIP_CELL_NOTE
}
OS version:
$(os_version | sed 's/^/  /')

Architecture: $(uname -m)
Rust version: $(rustc --version 2>&1)
Cargo version: $(cargo --version 2>&1)
Commit under test: $(git rev-parse HEAD 2>&1)
Branch or tag: ${GITHUB_HEAD_REF:-${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD 2>&1)}}
Test cycle: $scope

=== Commands run ===

${COMMANDS}
=== Results ===

${RESULTS}Client write: not run (needs coordinator approval; never run by this script)
Archive: not run
${SMOKE_OUTPUT:+
Smoke output:
$SMOKE_OUTPUT
}
=== Failures ===

${FAILURES:-none}

=== Privacy confirmation ===

No private keys / seed files / mnemonic phrases were read or printed: yes

Verdict: $verdict
EOF
}

SMOKE_OUTPUT=""
case "$MODE" in
    client)
        if is_windows_native; then
            echo "error: native Windows is not a SNIP client target (SNIP #49); use WSL2 (§ 3)" >&2
            exit 2
        fi
        ran "cargo --version; rustc --version; git rev-parse HEAD"
        if check_toolchain && build_and_test && [ "$SMOKE" -eq 1 ]; then
            smoke
        fi
        scope="read-only"
        [ "$SMOKE" -eq 1 ] || scope="read-only, build + tests only (smoke not run)"
        ;;
    windows-fail-closed)
        if ! is_windows_native; then
            echo "error: windows-fail-closed must run on native Windows (the tests compile only off Unix)" >&2
            exit 2
        fi
        ran "cargo --version; rustc --version; git rev-parse HEAD"
        windows_fail_closed
        scope="native Windows fail-closed (SNIP #49); no client cycle"
        ;;
    *) usage ;;
esac

if [ "$FAILED" -ne 0 ]; then
    verdict=FAIL
elif [ "$MODE" = client ] && [ "$SMOKE" -eq 0 ]; then
    verdict="PARTIAL (smoke not run; not a § 8 pass)"
else
    verdict=PASS
fi

REPORT=$(report "$verdict" "$scope")
printf '%s\n' "$REPORT"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    # shellcheck disable=SC2016  # a Markdown code fence, not a command
    printf '```text\n%s\n```\n' "$REPORT" >>"$GITHUB_STEP_SUMMARY"
fi
[ "$FAILED" -eq 0 ]
