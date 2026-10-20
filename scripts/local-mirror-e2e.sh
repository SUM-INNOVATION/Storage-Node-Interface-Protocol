#!/usr/bin/env bash
# Local-mirror E2E (docs/release/release-checklist.md § 5) against the
# chain's synthetic five-validator devnet, `deploy/snip-local-mirror.yaml`
# in the sum-chain repository. For a Docker-capable Linux host: the
# `release local-mirror E2E` workflow runs it on a fresh GitHub-hosted
# runner.
#
# Usage:
#   SUMCHAIN_DIR=<sum-chain checkout> SUMCHAIN_COMMIT=<full sha> \
#       scripts/local-mirror-e2e.sh
#
# What it does:
#   1. checks that the chain checkout is exactly SUMCHAIN_COMMIT and has
#      no devnet/ yet;
#   2. provisions the devnet with the chain's own provisioner
#      (`setup-local-testnet --mode topology --validator-count 5`) into the
#      chain checkout's ignored devnet/ directory: chain id 1337, V2 from
#      genesis, throwaway keys generated on this host;
#   3. builds the images from that commit and brings all five validators up;
#   4. writes SNIP's role seed files from the devnet's funded roles into a
#      0700 temporary directory outside both repositories;
#   5. runs the read-only smoke, then `make e2e-mirror`;
#   6. always tears the devnet down (`down -v`) and deletes devnet/ and the
#      seed files. Container logs are printed only on failure.
#
# The keys are synthetic, generated for this run and deleted at its end.
# Nothing here reaches mainnet or any shared network, and no private key is
# printed.

set -euo pipefail

: "${SUMCHAIN_DIR:?set SUMCHAIN_DIR to the sum-chain checkout}"
: "${SUMCHAIN_COMMIT:?set SUMCHAIN_COMMIT to the expected full chain commit}"

SNIP_DIR="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
SUMCHAIN_DIR="$(cd "$SUMCHAIN_DIR" && pwd)"
PROJECT="${E2E_COMPOSE_PROJECT:-snip-e2e}"
COMPOSE=(docker compose -p "$PROJECT" -f "$SUMCHAIN_DIR/deploy/snip-local-mirror.yaml")
RPC="http://localhost:8545"
READY_URL="http://localhost:8546/ready"
READY_TIMEOUT="${READY_TIMEOUT:-300}"

KEYS_DIR="$(mktemp -d)"
chmod 0700 "$KEYS_DIR"
started=0

# shellcheck disable=SC2329  # invoked by the EXIT trap below
cleanup() {
    local status=$?
    if [ "$started" = 1 ]; then
        if [ "$status" -ne 0 ]; then
            echo "--- container logs (last 200 lines each; failure only) ---" >&2
            "${COMPOSE[@]}" logs --no-color --tail 200 >&2 || true
        fi
        "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    fi
    rm -rf "$SUMCHAIN_DIR/devnet" "$KEYS_DIR"
    exit "$status"
}
trap cleanup EXIT

head="$(git -C "$SUMCHAIN_DIR" rev-parse HEAD)"
if [ "$head" != "$SUMCHAIN_COMMIT" ]; then
    echo "error: chain checkout is at $head, expected $SUMCHAIN_COMMIT" >&2
    exit 1
fi
if [ -e "$SUMCHAIN_DIR/devnet" ]; then
    echo "error: $SUMCHAIN_DIR/devnet already exists; refusing to touch it" >&2
    exit 1
fi

echo "== 1. provision the synthetic devnet at chain commit $SUMCHAIN_COMMIT"
(
    cd "$SUMCHAIN_DIR"
    mkdir -m 0700 devnet
    cargo run --locked --bin setup-local-testnet -- \
        --mode topology --validator-count 5 --output-dir devnet
)
# The containers run as uid 1000. The throwaway validator keys must be
# readable inside the 0700 devnet/ directory when this host's uid differs.
chmod 0644 "$SUMCHAIN_DIR"/devnet/keys/validator*.json

# Opt-in, devnet only: scenario 13 (reassignment and V2 re-fetch) needs an
# archive to leave the active set by its own BeginUnstake and the owner to
# reassign, so this throwaway genesis opens exactly two existing gates from
# genesis: archive unbonding and archive reassignment. PoR assignment
# targeting and the assignment-aware PoR scheduler stay unset, and this
# refuses to run if the provisioner ever set them. Only the generated
# devnet/genesis.json of this run is edited; nothing in either repository.
if [ "${SNIP_E2E_REASSIGNMENT:-0}" = 1 ]; then
    echo "== 1b. devnet-only opt-in: archive unbonding + reassignment gates at genesis"
    python3 - "$SUMCHAIN_DIR/devnet/genesis.json" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path) as f:
    genesis = json.load(f)
if genesis.get("chain_id") != 1337:
    sys.exit(f"refusing: chain id {genesis.get('chain_id')!r} is not the synthetic devnet's 1337")
params = genesis["params"]
for gate in (
    "por_assignment_targeting_enabled_from_height",
    "assignment_aware_por_scheduler_enabled_from_height",
):
    if params.get(gate) is not None:
        sys.exit(f"refusing: {gate} is set in the devnet genesis")
params["archive_unbonding_enabled_from_height"] = 0
params["archive_reassignment_enabled_from_height"] = 0
with open(path, "w") as f:
    json.dump(genesis, f, indent=2)
print("devnet genesis: archive_unbonding_enabled_from_height=0, "
      "archive_reassignment_enabled_from_height=0; PoR targeting and scheduler unset")
PY
fi

echo "== 2. build and start the five validators"
started=1
GIT_HASH="$SUMCHAIN_COMMIT" "${COMPOSE[@]}" up -d --build --wait --wait-timeout "$READY_TIMEOUT"

deadline=$(( $(date +%s) + READY_TIMEOUT ))
until curl -fsS "$READY_URL" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "error: $READY_URL not ready within ${READY_TIMEOUT}s" >&2
        exit 1
    fi
    sleep 2
done
echo "validator-1 ready"

echo "== 3. SNIP role seeds from the devnet's funded roles"
python3 - "$SUMCHAIN_DIR/devnet/keys" "$KEYS_DIR" <<'PY'
import json
import os
import sys

src, dst = sys.argv[1], sys.argv[2]
# SNIP role -> funded devnet role (scripts/src/lib.rs in sum-chain).
roles = {
    "owner": "client",
    "recipient": "verifier",
    "third_party": "funder",
    "archive_1": "archive1",
    "archive_2": "archive2",
    "archive_3": "archive3",
}
for snip_role, chain_role in roles.items():
    with open(os.path.join(src, chain_role + ".json")) as f:
        seed = bytes(json.load(f))
    if len(seed) != 32:
        sys.exit(f"{chain_role}: seed is {len(seed)} bytes, expected 32")
    path = os.path.join(dst, snip_role + ".seed.hex")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(seed.hex() + "\n")
print(f"wrote {len(roles)} role seed files")
PY

cd "$SNIP_DIR"

echo "== 4. read-only smoke against the devnet"
make smoke RPC="$RPC" SMOKE_ARGS=--require-v2

echo "== 5. local-mirror E2E suite"
suite=0
# Scenario 13 removes an archive from the active set, so it never runs inside
# the shared suite: the suite runs with the opt-in unset, then scenario 13
# runs alone, last, with its own time limit.
env -u SNIP_E2E_REASSIGNMENT SNIP_E2E_RPC_URL="$RPC" SNIP_E2E_KEYS_DIR="$KEYS_DIR" \
    make e2e-mirror || suite=$?
if [ "${SNIP_E2E_REASSIGNMENT:-0}" = 1 ]; then
    # Runs whatever the shared suite's result, so its own evidence is always
    # produced; the job still fails if either part failed.
    echo "== 5b. scenario 13 alone: reassignment, public re-fetch, restart, corruption, Private"
    scenario=0
    SNIP_E2E_REASSIGNMENT=1 SNIP_E2E_RPC_URL="$RPC" SNIP_E2E_KEYS_DIR="$KEYS_DIR" \
        timeout 1500 cargo test -p sum-node --test e2e_mirror -- --ignored --exact \
        reassignment_refetch_restores_public_coverage --nocapture || scenario=$?
    echo "shared suite exit: $suite; scenario 13 exit: $scenario"
    if [ "$suite" -eq 0 ]; then
        suite=$scenario
    fi
fi
if [ "$suite" -eq 0 ]; then
    echo "local-mirror E2E: ok (chain commit $SUMCHAIN_COMMIT)"
    exit 0
fi

# Diagnostics, on failure only. The suite discards its archives' output, so
# repeat its basic flow by hand with every log kept: three archives, one
# Public ingest, then print the ingest output and the archives' warnings.
# Synthetic keys only; the logs carry peer ids and devnet addresses, never
# key material.
echo "== 6. diagnostics: one Public ingest with archive logs kept" >&2
SUM_NODE="$SNIP_DIR/target/debug/sum-node"
DIAG="$(mktemp -d)"
pids=()
for i in 1 2 3; do
    RUST_LOG=info "$SUM_NODE" --key-file "$KEYS_DIR/archive_$i.seed.hex" \
        --store-dir "$DIAG/archive_$i" --rpc-url "$RPC" --chain-id 1337 listen \
        >"$DIAG/archive_$i.log" 2>&1 &
    pids+=("$!")
done
sleep 15
printf 'local-mirror diagnostics %s\n' "$(date -u +%s)" >"$DIAG/probe.txt"
head -c 4096 /dev/urandom >>"$DIAG/probe.txt"
RUST_LOG=info timeout 300 "$SUM_NODE" --key-file "$KEYS_DIR/owner.seed.hex" \
    --store-dir "$DIAG/owner" --rpc-url "$RPC" --chain-id 1337 \
    ingest-v2 "$DIAG/probe.txt" --visibility public >"$DIAG/ingest.log" 2>&1 \
    && echo "diagnostic ingest exit: 0" >&2 \
    || echo "diagnostic ingest exit: $?" >&2
kill "${pids[@]}" 2>/dev/null || true
wait 2>/dev/null || true
echo "--- diagnostic ingest output (last 80 lines) ---" >&2
tail -n 80 "$DIAG/ingest.log" >&2 || true
for i in 1 2 3; do
    echo "--- archive_$i: warnings, errors and V2 push lines (last 60) ---" >&2
    grep -E "WARN|ERROR|refus|[Pp]ush|ManifestPush|attest" "$DIAG/archive_$i.log" | tail -n 60 >&2 || true
done
rm -rf "$DIAG"
exit "$suite"
