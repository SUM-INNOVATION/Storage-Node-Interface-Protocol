#!/usr/bin/env bash
# audit-logs.sh — privacy-leak guardrail (WS4).
#
# Scans `crates/**/*.rs` for forbidden secret tokens appearing inside
# log-macro lines (info!, warn!, error!, debug!, trace!, eprintln!,
# println!). Exits non-zero on any violation.
#
# This is a guardrail, NOT proof of correctness. The behavioral
# fail-closed unit tests in `crates/sum-node` are the load-bearing
# privacy guarantees; this script only catches accidental token
# regressions in log strings.
#
# Allowlist: append `// audit-allow: <one-word-reason>` to a log line
# to opt that line out. Forces every exemption to state a reason so
# reviewers see it in PR diffs.
#
# Forbidden tokens (case-sensitive substring match within a log-macro
# line). Per-token allowlist of legitimate non-secret identifiers
# (e.g. `blake3_seed`, `plaintext_blake3_hash`) suppresses common
# false positives. Anything else with the bare token must opt out
# explicitly.
#
# Usage:
#   scripts/audit-logs.sh              # scan crates/
#   scripts/audit-logs.sh <dir>        # scan a custom dir
#   scripts/audit-logs.sh --self-test  # verify the script catches a
#                                      #   known-bad input

set -euo pipefail

# Deterministic scan: byte-locale collation, sorted file enumeration,
# stable output ordering. Two runs against the same checkout MUST
# produce byte-identical stdout.
export LC_ALL=C
export LANG=C

# Forbidden tokens. Each entry is `TOKEN|ALLOWLIST_REGEX`. Lines
# matching the allowlist regex (in addition to the token) are
# exempted — used to suppress legitimate identifiers like
# `blake3_seed` that contain the token but aren't the secret.
TOKENS=(
  "k_file|"
  "K_file|"
  "x25519_secret|"
  "bundle_hex|"
  "encrypted_key_bundle|"
  "seed|blake3_seed|random_seed|seed_phrase|seed_bytes_for|peer_id_seed"
  "plaintext|plaintext_blake3_hash|plaintext_size_bytes|plaintext_hash|plaintext_chunk_count"
)

# Log macros we care about. Bare names (info, warn, …) plus the
# tracing:: qualified forms. `eprintln!` and `println!` included so
# stderr/stdout writes can't sneak past the tracing layer.
LOG_MACRO_RE='\b(info|warn|error|debug|trace|eprintln|println)!\s*\('

# Allowlist marker that opts a single line out of the audit. The
# reason word is required so reviewers see it in diffs.
ALLOW_MARKER_RE='//[[:space:]]*audit-allow:[[:space:]]*[A-Za-z0-9_-]+'

scan_dir() {
  local dir="$1"
  local violations=0
  # Sorted file list for deterministic output. NUL-separation handles
  # any path with whitespace defensively (no such files exist today,
  # but a future contributor adding one shouldn't break the audit).
  local files
  files=$(find "$dir" -type f -name '*.rs' -print0 | LC_ALL=C sort -z | xargs -0 -I{} echo "{}")

  for entry in "${TOKENS[@]}"; do
    local token="${entry%%|*}"
    local allowlist="${entry#*|}"
    # Combine the macro-match anchor and the token into a single
    # extended regex. grep -nE prints `path:line:content`.
    local hits
    hits=$(echo "$files" | xargs grep -nHE "${LOG_MACRO_RE}.*\b${token}\b" 2>/dev/null || true)
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      # Inline allowlist marker → skip.
      if echo "$line" | grep -qE "$ALLOW_MARKER_RE"; then
        continue
      fi
      # Per-token contextual allowlist (e.g. `blake3_seed`) → skip.
      if [ -n "$allowlist" ] && echo "$line" | grep -qE "($allowlist)"; then
        continue
      fi
      printf 'audit-logs: violation [token=%s]: %s\n' "$token" "$line" >&2
      violations=$((violations + 1))
    done <<<"$hits"
  done
  echo "$violations"
}

# ── Self-test mode ──────────────────────────────────────────────────
# Creates a temp dir with one known-bad log line and verifies that the
# script flags it. Without this, "the script returned 0 violations"
# is indistinguishable from "the script never matches anything." The
# self-test runs the same scan_dir code path against a controlled
# input.
if [ "${1:-}" = "--self-test" ]; then
  tmpdir=$(mktemp -d)
  trap 'rm -rf "$tmpdir"' EXIT
  cat > "$tmpdir/bad.rs" <<'EOF'
fn leak() {
    let k_file = [0u8; 32];
    info!("derived k_file = {:?}", k_file);
}
EOF
  hits=$(scan_dir "$tmpdir" 2>&1 >/dev/null || true)
  scan_violations=$(scan_dir "$tmpdir" 2>/dev/null)
  if [ "$scan_violations" -ge 1 ]; then
    echo "audit-logs: self-test passed — script catches a known-bad k_file log line."
    exit 0
  else
    echo "audit-logs: self-test FAILED — script did not flag the synthetic violation." >&2
    echo "$hits" >&2
    exit 1
  fi
fi

# ── Production scan ──────────────────────────────────────────────────
target_dir="${1:-crates}"
if [ ! -d "$target_dir" ]; then
  echo "audit-logs: target dir not found: $target_dir" >&2
  exit 1
fi
echo "audit-logs: scanning $target_dir for forbidden tokens in log macros (LC_ALL=$LC_ALL)…"
violations=$(scan_dir "$target_dir")
if [ "$violations" -gt 0 ]; then
  printf 'audit-logs: %d violation(s) found.\n' "$violations" >&2
  exit 1
fi
echo "audit-logs: clean — 0 violations."
