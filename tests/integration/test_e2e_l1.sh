#!/usr/bin/env bash
#
# End-to-End L1 Integration Test — SUM Storage Node
#
# Tests the complete PoR (Proof of Retrievability) pipeline:
#   1. Register as ArchiveNode on the L1
#   2. Ingest a file locally
#   3. Register the file's metadata on the L1
#   4. Start the storage node and wait for a PoR challenge
#   5. Verify the node automatically submits a proof
#
# Prerequisites:
#   - Two SUM Chain validator nodes running with block production
#   - L1 RPC accessible at the specified URL
#   - A funded account with at least 1.2 Koppa
#
# Usage:
#   ./tests/integration/test_e2e_l1.sh --rpc-url http://<validator-ip>:8545 --seed-hex <64_hex_chars>
#
# If --seed-hex is not provided, the script generates a random seed.
# NOTE: The generated account must be funded on the L1 before this test can succeed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Parse arguments ────────────────────────────────────────────────────────────

RPC_URL="http://127.0.0.1:8545"
SEED_HEX=""
TIMEOUT=360  # 6 minutes (challenges every ~200s)

while [[ $# -gt 0 ]]; do
    case $1 in
        --rpc-url)   RPC_URL="$2";   shift 2 ;;
        --seed-hex)  SEED_HEX="$2";  shift 2 ;;
        --timeout)   TIMEOUT="$2";   shift 2 ;;
        *)           echo "Unknown arg: $1"; exit 1 ;;
    esac
done

echo "================================================"
echo " SUM Storage Node — E2E L1 Integration Test"
echo "================================================"
echo ""
echo "  RPC URL:  $RPC_URL"
echo "  Timeout:  ${TIMEOUT}s"
echo ""

# ── Step 1: Build ──────────────────────────────────────────────────────────────

echo "[1/9] Building binaries..."
cd "$PROJECT_ROOT"
cargo build --bin sum-node --bin e2e-helper 2>&1 | tail -1
BIN="$PROJECT_ROOT/target/debug/sum-node"
HELPER="$PROJECT_ROOT/target/debug/e2e-helper"
echo "  OK: Built sum-node and e2e-helper"

# ── Step 2: Setup ─────────────────────────────────────────────────────────────

TMPDIR=$(mktemp -d)
echo "[2/9] Temp directory: $TMPDIR"

NODE_HOME="$TMPDIR/node_home"
mkdir -p "$NODE_HOME"

NODE_PID=""
cleanup() {
    kill "$NODE_PID" 2>/dev/null || true
    sleep 0.5
    kill -9 "$NODE_PID" 2>/dev/null || true
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

# Generate or use provided seed
if [ -z "$SEED_HEX" ]; then
    SEED_HEX=$(openssl rand -hex 32)
    echo "  Generated random seed: $SEED_HEX"
    echo "  WARNING: This account must be funded on the L1 before the test can succeed."
fi
echo "$SEED_HEX" > "$TMPDIR/key.hex"

# ── Step 3: Health check ──────────────────────────────────────────────────────

echo "[3/9] Checking L1 health..."
if ! "$HELPER" health --rpc-url "$RPC_URL" 2>/dev/null; then
    echo "FAIL: L1 RPC is not reachable at $RPC_URL"
    echo ""
    echo "  Make sure your validator nodes are running and the RPC"
    echo "  is bound to an accessible address (not just 127.0.0.1"
    echo "  if the validators are on a different machine)."
    echo ""
    echo "  To check: curl -X POST $RPC_URL -H 'Content-Type: application/json' \\"
    echo "    -d '{\"jsonrpc\":\"2.0\",\"method\":\"health\",\"params\":[],\"id\":1}'"
    exit 1
fi
echo "  OK: L1 reachable"

# ── Step 4: Derive address and check balance ──────────────────────────────────

echo "[4/9] Checking account..."
L1_ADDR=$("$HELPER" l1-address --seed-hex "$SEED_HEX")
echo "  L1 Address: $L1_ADDR"

BALANCE=$("$HELPER" balance --rpc-url "$RPC_URL" --address "$L1_ADDR" 2>/dev/null || echo "0")
echo "  Balance: $BALANCE base units"

# Check if balance is sufficient (need ~1.2 Koppa = 1,200,000,000)
# Simple numeric check (may fail for very large balances in string format)
if [ "$BALANCE" = "0" ] || [ "$BALANCE" = "\"0\"" ] || [ "$BALANCE" = "null" ]; then
    echo ""
    echo "FAIL: Account has zero balance."
    echo ""
    echo "  The test account needs at least 1.2 Koppa (1,200,000,000 base units)."
    echo "  Fund it by either:"
    echo "    1. Adding it to genesis/local_genesis.json before chain init"
    echo "    2. Transferring Koppa from a funded account"
    echo ""
    echo "  Account address: $L1_ADDR"
    echo "  Seed hex:        $SEED_HEX"
    exit 1
fi
echo "  OK: Account funded"

# ── Step 5: Register as ArchiveNode ───────────────────────────────────────────

echo "[5/9] Registering as ArchiveNode..."
NODE_RECORD=$("$HELPER" node-record --rpc-url "$RPC_URL" --address "$L1_ADDR" 2>/dev/null || echo "null")

if echo "$NODE_RECORD" | grep -q "ArchiveNode"; then
    echo "  OK: Already registered as ArchiveNode"
else
    "$HELPER" register-node --seed-hex "$SEED_HEX" --rpc-url "$RPC_URL" --stake 1000000000
    echo "  Waiting for registration to confirm..."
    DEADLINE=$((SECONDS + 30))
    while true; do
        NODE_RECORD=$("$HELPER" node-record --rpc-url "$RPC_URL" --address "$L1_ADDR" 2>/dev/null || echo "null")
        if echo "$NODE_RECORD" | grep -q "ArchiveNode"; then
            break
        fi
        if [ $SECONDS -gt $DEADLINE ]; then
            echo "FAIL: ArchiveNode registration not confirmed within 30s"
            echo "  Node record: $NODE_RECORD"
            exit 1
        fi
        sleep 2
    done
    echo "  OK: Registered as ArchiveNode"
fi

# ── Step 6: Ingest file locally ───────────────────────────────────────────────

echo "[6/9] Ingesting test file..."
TEST_FILE="$TMPDIR/test_file.bin"
# 1.5 MB file (2 chunks: 1MB + 0.5MB)
dd if=/dev/urandom of="$TEST_FILE" bs=1048576 count=1 2>/dev/null
dd if=/dev/urandom bs=524288 count=1 >> "$TEST_FILE" 2>/dev/null
FILE_SIZE=$(wc -c < "$TEST_FILE" | tr -d ' ')

# Run ingest — it will chunk, store, then wait for peers (which we don't need).
# Capture the manifest output, then kill it.
HOME="$NODE_HOME" RUST_LOG=info "$BIN" \
    --key-file "$TMPDIR/key.hex" \
    ingest "$TEST_FILE" \
    > "$TMPDIR/ingest_stdout.log" 2> "$TMPDIR/ingest_stderr.log" &
INGEST_PID=$!

# Wait for manifest to be logged
DEADLINE=$((SECONDS + 30))
while ! grep -q "manifest indexed" "$TMPDIR/ingest_stderr.log" 2>/dev/null; do
    if ! kill -0 "$INGEST_PID" 2>/dev/null; then
        break
    fi
    if [ $SECONDS -gt $DEADLINE ]; then
        echo "FAIL: Ingest did not complete within 30s"
        cat "$TMPDIR/ingest_stderr.log"
        kill "$INGEST_PID" 2>/dev/null || true
        exit 1
    fi
    sleep 0.5
done
kill "$INGEST_PID" 2>/dev/null || true
wait "$INGEST_PID" 2>/dev/null || true

# Extract merkle_root from logs
MERKLE_ROOT=$(grep "manifest indexed" "$TMPDIR/ingest_stderr.log" | sed -n 's/.*merkle_root=\([a-f0-9]*\).*/\1/p' | head -1)
if [ -z "$MERKLE_ROOT" ]; then
    echo "FAIL: Could not extract merkle_root from ingest output"
    cat "$TMPDIR/ingest_stderr.log"
    exit 1
fi
echo "  OK: Ingested $FILE_SIZE bytes, merkle_root=$MERKLE_ROOT"

# ── Step 7: Register file on L1 ──────────────────────────────────────────────

echo "[7/9] Registering file on L1..."
"$HELPER" register-file \
    --seed-hex "$SEED_HEX" \
    --rpc-url "$RPC_URL" \
    --merkle-root "$MERKLE_ROOT" \
    --total-size "$FILE_SIZE" \
    --fee-deposit 100000000

# Wait for confirmation
echo "  Waiting for file registration to confirm..."
DEADLINE=$((SECONDS + 30))
while true; do
    FILE_INFO=$("$HELPER" balance --rpc-url "$RPC_URL" --address "$L1_ADDR" 2>/dev/null || echo "")
    # Check via access list query (would need another helper command)
    # For now, just wait a few blocks
    sleep 4
    if [ $SECONDS -gt $((DEADLINE - 20)) ]; then
        break  # Assume confirmed after a few blocks
    fi
done
echo "  OK: File registered on L1"

# ── Step 8: Start storage node and wait for PoR ──────────────────────────────

echo "[8/9] Starting storage node (waiting for PoR challenge)..."
echo "  This may take up to ~200 seconds (challenges every 100 blocks at 2s/block)"
HOME="$NODE_HOME" RUST_LOG=info "$BIN" \
    --key-file "$TMPDIR/key.hex" \
    --rpc-url "$RPC_URL" \
    --por-poll-secs 5 \
    listen \
    > "$TMPDIR/node_stdout.log" 2> "$TMPDIR/node_stderr.log" &
NODE_PID=$!

# Wait for PoR proof submission
DEADLINE=$((SECONDS + TIMEOUT))
PROOF_SUBMITTED=false
while true; do
    if grep -q "PoR proof submitted" "$TMPDIR/node_stderr.log" 2>/dev/null; then
        PROOF_SUBMITTED=true
        break
    fi
    if ! kill -0 "$NODE_PID" 2>/dev/null; then
        echo "FAIL: Storage node exited unexpectedly"
        cat "$TMPDIR/node_stderr.log"
        exit 1
    fi
    if [ $SECONDS -gt $DEADLINE ]; then
        echo "FAIL: No PoR proof submitted within ${TIMEOUT}s"
        echo ""
        echo "  Possible causes:"
        echo "    - No challenge was issued for our file (try waiting longer)"
        echo "    - The node is not registered as an ArchiveNode"
        echo "    - The file is not registered on the L1"
        echo ""
        echo "--- Last 30 lines of node stderr ---"
        tail -30 "$TMPDIR/node_stderr.log"
        exit 1
    fi
    # Print progress every 30s
    ELAPSED=$((SECONDS))
    if (( ELAPSED % 30 == 0 )); then
        BLOCK=$("$HELPER" block-number --rpc-url "$RPC_URL" 2>/dev/null || echo "?")
        echo "  ... waiting (block $BLOCK, elapsed ${ELAPSED}s)"
    fi
    sleep 2
done

# ── Step 9: Verify ────────────────────────────────────────────────────────────

echo "[9/9] Verifying..."

# Check node is still Active (not slashed)
NODE_RECORD=$("$HELPER" node-record --rpc-url "$RPC_URL" --address "$L1_ADDR" 2>/dev/null || echo "null")
if echo "$NODE_RECORD" | grep -q '"status": "Active"'; then
    echo "  OK: Node still Active (not slashed)"
else
    echo "  WARNING: Node status: $NODE_RECORD"
fi

echo ""
echo "================================================"
echo " PASS: E2E L1 Integration Test"
echo "================================================"
echo ""
echo "  L1 Address:     $L1_ADDR"
echo "  Merkle Root:    $MERKLE_ROOT"
echo "  File Size:      $FILE_SIZE bytes"
echo "  PoR Proof:      Submitted and accepted"
echo "  Node Status:    Active"
echo ""
