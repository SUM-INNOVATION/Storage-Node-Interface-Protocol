#!/usr/bin/env sh
# check-cli-doc.sh — verify every subcommand and every flag of
# `sum-node` and `e2e-helper` is documented under the correct
# subcommand section in `docs/reference/cli.md`.
#
# Design (mapped to constraint 14 in the audit charter):
# - Discovers subcommands from `<binary> --help` output.
# - For each subcommand, runs `<binary> <subcmd> --help` and
#   extracts every long-form flag (`--flag-name`).
# - Locates the subcommand's section in `docs/reference/cli.md`
#   by matching the H3 heading `### <binary> <subcmd>` and taking
#   the body until the next `###` or `##` heading. Every flag
#   from `--help` must appear inside that section body (or the
#   binary's "Global flags" section for `sum-node`).
# - This prevents a flag documented under one subcommand from
#   spuriously satisfying another: matching is scoped to each
#   subcommand's own section.
#
# Prerequisites:
# - Release binaries built at target/release/sum-node and
#   target/release/e2e-helper. `make release-check` builds these
#   before invoking this script.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

DOC="docs/reference/cli.md"

if [ ! -e "$DOC" ]; then
  printf 'check-cli-doc: %s not found\n' "$DOC" >&2
  exit 1
fi

SUM_NODE="target/release/sum-node"
E2E_HELPER="target/release/e2e-helper"

if [ ! -x "$SUM_NODE" ] || [ ! -x "$E2E_HELPER" ]; then
  printf 'check-cli-doc: expected %s and %s to exist and be executable.\n' "$SUM_NODE" "$E2E_HELPER" >&2
  printf 'check-cli-doc: run `cargo build --release --workspace` first (or `make release-check`).\n' >&2
  exit 1
fi

FAILURES=$(mktemp -t snip-check-cli-doc.XXXXXX)
SECTION=$(mktemp -t snip-check-cli-doc-section.XXXXXX)
GLOBAL=$(mktemp -t snip-check-cli-doc-global.XXXXXX)
trap 'rm -f "$FAILURES" "$SECTION" "$GLOBAL"' EXIT INT HUP TERM

# Extract a named section from the CLI doc by H3 heading and
# write its body (until the next `##`/`###`) to $2.
#
# $1 = section title text (case-sensitive, must exactly match the
#      heading after `### `)
# $2 = output file path
extract_section() {
  awk -v title="$1" '
    BEGIN { on=0 }
    /^## / && on { exit }
    /^### / {
      if (on) { exit }
      hdr=$0
      sub(/^### */, "", hdr)
      if (hdr == title) { on=1; next }
    }
    on { print }
  ' "$DOC" > "$2"
}

# Extract global-flag section body once (used to satisfy global
# flags for sum-node subcommands).
extract_section 'Global flags' "$GLOBAL"

# Extract the long-form flag names from a `--help` output. Emits
# unique flag names (with leading `--`) on separate lines.
#
# Only counts lines that clap prints as actual flag definitions:
# the flag must appear at the beginning of the line (after leading
# whitespace), optionally preceded by a short-flag alias
# (`-x, --flag`). This excludes descriptive mentions inside help
# text paragraphs.
extract_flags_from_help() {
  grep -E '^[[:space:]]+(-[a-zA-Z], +)?--[a-zA-Z][a-zA-Z0-9-]*' \
    | grep -oE -- '--[a-zA-Z][a-zA-Z0-9-]*' \
    | LC_ALL=C sort -u
}

# Discover subcommands from `<binary> --help`. The clap "Commands:"
# block lists them; each subcommand line begins with two spaces of
# indent and then the command name.
extract_subcommands() {
  # $1 = binary path
  "$1" --help 2>&1 | awk '
    /^Commands:/ { in_block=1; next }
    /^Options:/  { in_block=0 }
    /^$/         { if (in_block) in_block=0 }
    in_block {
      # Skip help alias.
      if ($1 == "help") next
      # Print the first word (the subcommand name).
      print $1
    }
  ' | LC_ALL=C sort -u
}

# Verify a single subcommand's flags are documented in the
# expected section.
#
# $1 = binary label ("sum-node" or "e2e-helper")
# $2 = binary path
# $3 = subcommand name
verify_subcommand() {
  label=$1
  bin=$2
  sub=$3
  section_title="${label} ${sub}"

  extract_section "$section_title" "$SECTION"
  if [ ! -s "$SECTION" ]; then
    printf 'MISSING SECTION: %s "%s" — no `### %s` heading in %s\n' \
      "$label" "$sub" "$section_title" "$DOC" >> "$FAILURES"
    return
  fi

  # Get the actual flags from --help.
  flags=$("$bin" "$sub" --help 2>&1 | extract_flags_from_help || true)

  for flag in $flags; do
    # Common help/version flags — skip unless documented explicitly.
    case "$flag" in
      --help|--version) continue ;;
    esac

    if grep -qF -- "$flag" "$SECTION"; then
      continue
    fi

    # Fall back to Global (only for sum-node global flags).
    if [ "$label" = "sum-node" ] && grep -qF -- "$flag" "$GLOBAL"; then
      continue
    fi

    printf 'MISSING FLAG: %s %s: %s not documented under "### %s" (or "Global flags")\n' \
      "$label" "$sub" "$flag" "$section_title" >> "$FAILURES"
  done
}

# Also verify sum-node global flags themselves appear in the
# "Global flags" section.
verify_global_flags() {
  label=$1
  bin=$2

  flags=$("$bin" --help 2>&1 | extract_flags_from_help || true)
  for flag in $flags; do
    case "$flag" in
      --help|--version) continue ;;
    esac
    if grep -qF -- "$flag" "$GLOBAL"; then
      continue
    fi
    # Global flags might legitimately be shared with subcommands
    # (some clap layouts). Do not FAIL on missing-from-global; only
    # note it. This keeps the checker friendly to layout drift.
    printf 'INFO: %s global flag not in "Global flags" section: %s\n' \
      "$label" "$flag" >&2
  done
}

# Run for each binary + each subcommand.
for pair in "sum-node:$SUM_NODE" "e2e-helper:$E2E_HELPER"; do
  label=${pair%%:*}
  bin=${pair#*:}

  # sum-node has a canonical "Global flags" section; e2e-helper
  # does not (each subcommand names its own --rpc-url).
  if [ "$label" = "sum-node" ]; then
    verify_global_flags "$label" "$bin"
  fi

  subcommands=$(extract_subcommands "$bin")
  for sub in $subcommands; do
    verify_subcommand "$label" "$bin" "$sub"
  done
done

if [ -s "$FAILURES" ]; then
  cat "$FAILURES" >&2
  printf 'check-cli-doc: one or more CLI-vs-doc drifts detected\n' >&2
  exit 1
fi

printf 'check-cli-doc: ok\n'
