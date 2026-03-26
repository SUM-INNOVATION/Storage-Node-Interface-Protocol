#!/usr/bin/env bash
#
# P2P Integration Test — SUM Storage Node
#
# Tests the complete chunk transfer pipeline between two nodes on localhost:
#   Node A: ingest file → chunk → announce → serve
#   Node B: discover → fetch → verify → store
#
# No L1 blockchain required. Uses mDNS for peer discovery.
#
# Usage:
#   ./tests/integration/test_p2p.sh
#
# Requirements:
#   - Rust toolchain (cargo)
#   - openssl (for key generation)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TIMEOUT=60

echo "========================================"
echo " SUM Storage Node — P2P Integration Test"
echo "========================================"
echo ""

# ── Step 1: Build ──────────────────────────────────────────────────────────────

echo "[1/7] Building sum-node..."
cd "$PROJECT_ROOT"
cargo build --bin sum-node 2>&1 | tail -1
BIN="$PROJECT_ROOT/target/debug/sum-node"

if [ ! -f "$BIN" ]; then
    echo "FAIL: sum-node binary not found at $BIN"
    exit 1
fi
echo "  OK: $BIN"

# ── Step 2: Setup temp directories ─────────────────────────────────────────────

TMPDIR=$(mktemp -d)
echo "[2/7] Temp directory: $TMPDIR"

NODE_A_HOME="$TMPDIR/node_a_home"
NODE_B_HOME="$TMPDIR/node_b_home"
mkdir -p "$NODE_A_HOME" "$NODE_B_HOME"

# Cleanup on exit
cleanup() {
    kill "$NODE_A_PID" 2>/dev/null || true
    kill "$NODE_B_PID" 2>/dev/null || true
    # Give processes time to die
    sleep 0.5
    kill -9 "$NODE_A_PID" 2>/dev/null || true
    kill -9 "$NODE_B_PID" 2>/dev/null || true
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

NODE_A_PID=""
NODE_B_PID=""

# Kill any leftover sum-node processes from previous runs (stale mDNS cache)
pkill -f "sum-node" 2>/dev/null || true
sleep 1

# ── Step 3: Generate keys and test file ────────────────────────────────────────

echo "[3/7] Generating keys and test file..."
openssl rand -hex 32 > "$TMPDIR/key_a.hex"
openssl rand -hex 32 > "$TMPDIR/key_b.hex"

# Create a 2.5 MB test file (3 chunks: 1MB + 1MB + 0.5MB)
TEST_FILE="$TMPDIR/test_file.bin"
dd if=/dev/urandom of="$TEST_FILE" bs=1048576 count=2 2>/dev/null
dd if=/dev/urandom bs=524288 count=1 >> "$TEST_FILE" 2>/dev/null
FILE_SIZE=$(wc -c < "$TEST_FILE" | tr -d ' ')
echo "  OK: Test file $FILE_SIZE bytes (expecting 2621440)"

# ── Step 4: Start Node A (ingest mode) ────────────────────────────────────────

echo "[4/7] Starting Node A (ingest)..."
# NO_COLOR=1 disables ANSI escape codes in tracing output for clean grep
HOME="$NODE_A_HOME" NO_COLOR=1 RUST_LOG=info "$BIN" \
    --key-file "$TMPDIR/key_a.hex" \
    ingest "$TEST_FILE" \
    > "$TMPDIR/node_a.log" 2>&1 &
NODE_A_PID=$!

# Wait for Node A to finish ingesting (look for manifest or timeout message)
echo "  Waiting for Node A to ingest..."
DEADLINE=$((SECONDS + 40))
while true; do
    if grep -q "all chunks announced\|timed out waiting for peer\|listening for requests" "$TMPDIR/node_a.log" 2>/dev/null; then
        break
    fi
    if ! kill -0 "$NODE_A_PID" 2>/dev/null; then
        echo "FAIL: Node A exited unexpectedly"
        echo "--- Node A stderr ---"
        cat "$TMPDIR/node_a.log"
        exit 1
    fi
    if [ $SECONDS -gt $DEADLINE ]; then
        echo "FAIL: Node A did not finish ingesting within 40s"
        echo "--- Node A stderr ---"
        cat "$TMPDIR/node_a.log"
        exit 1
    fi
    sleep 0.5
done
echo "  OK: Node A ingested and serving"

# ── Step 5: Extract CID from Node A's manifest ────────────────────────────────

echo "[5/7] Extracting CID from manifest..."
# The manifest JSON is embedded in tracing output. Extract first CID.
CID=$(sed -n 's/.*"cid": "\([^"]*\)".*/\1/p' "$TMPDIR/node_a.log" | head -1)
if [ -z "$CID" ]; then
    # Try alternate pattern (single-line JSON)
    CID=$(grep -o '"cid":"[^"]*"' "$TMPDIR/node_a.log" | head -1 | sed 's/"cid":"//;s/"//')
fi
if [ -z "$CID" ]; then
    echo "FAIL: Could not extract CID from Node A's output"
    echo "--- Node A stderr ---"
    cat "$TMPDIR/node_a.log"
    exit 1
fi
echo "  OK: CID = $CID"

# ── Step 6: Start Node B (fetch mode) ─────────────────────────────────────────

# Brief pause to let stale mDNS cache entries expire
sleep 2

echo "[6/7] Starting Node B (fetch $CID)..."
HOME="$NODE_B_HOME" NO_COLOR=1 RUST_LOG=info "$BIN" \
    --key-file "$TMPDIR/key_b.hex" \
    fetch "$CID" \
    > "$TMPDIR/node_b.log" 2>&1 &
NODE_B_PID=$!

# Wait for fetch to complete
DEADLINE=$((SECONDS + 40))
while true; do
    if grep -q "chunk fetched successfully\|chunk already exists" "$TMPDIR/node_b.log" 2>/dev/null; then
        break
    fi
    if ! kill -0 "$NODE_B_PID" 2>/dev/null; then
        # Process exited — check if it was successful
        if grep -q "chunk fetched successfully\|chunk already exists" "$TMPDIR/node_b.log" 2>/dev/null; then
            break
        fi
        echo "FAIL: Node B exited without fetching"
        echo "--- Node B stderr ---"
        cat "$TMPDIR/node_b.log"
        exit 1
    fi
    if [ $SECONDS -gt $DEADLINE ]; then
        echo "FAIL: Node B did not fetch chunk within 40s"
        echo "--- Node B stderr ---"
        cat "$TMPDIR/node_b.log"
        echo "--- Node A stderr (for context) ---"
        cat "$TMPDIR/node_a.log"
        exit 1
    fi
    sleep 0.5
done
echo "  OK: Node B fetched chunk"

# ── Step 7: Verify ─────────────────────────────────────────────────────────────

echo "[7/7] Verifying chunk on disk..."
CHUNK_FILE="$NODE_B_HOME/.sumnode/store/$CID.chunk"
if [ -f "$CHUNK_FILE" ]; then
    CHUNK_SIZE=$(wc -c < "$CHUNK_FILE" | tr -d ' ')
    echo "  OK: Chunk file exists ($CHUNK_SIZE bytes)"
else
    echo "FAIL: Chunk file not found at $CHUNK_FILE"
    echo "  Contents of Node B store:"
    ls -la "$NODE_B_HOME/.sumnode/store/" 2>/dev/null || echo "  (store dir does not exist)"
    exit 1
fi

# ── Result ─────────────────────────────────────────────────────────────────────

echo ""
echo "========================================"
echo " PASS: P2P Integration Test"
echo "========================================"
echo ""
echo "  Test file:      $FILE_SIZE bytes (3 chunks)"
echo "  Node A:         Ingested, announced, served"
echo "  Node B:         Discovered, fetched, verified"
echo "  Chunk CID:      $CID"
echo "  Chunk on disk:  $CHUNK_FILE ($CHUNK_SIZE bytes)"
echo ""
