#!/usr/bin/env sh
# check-docs-links.sh — verify every relative Markdown link inside
# docs/**/*.md, README.md, and CHANGELOG.md points at a file that
# exists on disk.
#
# Design notes (mapped to constraint 13 in the audit charter):
# - Resolves paths relative to the containing Markdown file.
# - Ignores external URLs (http:, https:, mailto:), pure fragment
#   links (#foo), and code-formatted cross-repository paths
#   (`sum-chain:...` — used to reference the sum-chain repo).
# - Strips ?query strings and #fragment anchors before the
#   filesystem check.
# - Supports paths containing simple URL-encoded characters (%20
#   for space, %28 / %29 for parens).
# - Failure output: "BROKEN: <file>:<line>: <target> (resolved to
#   <path>)" — source file, line, and unresolved target. Exit 1 on
#   any failure.
# - Does NOT check inline-code backtick paths (those are the
#   sum-chain: convention's home) — only markdown links.
#
# Intended to be run inside `make release-check`. Also runnable
# standalone from the repo root.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Collect failures into a file so subshell status can propagate.
FAILURES=$(mktemp -t snip-check-docs-links.XXXXXX)
trap 'rm -f "$FAILURES"' EXIT INT HUP TERM

# Simple URL-decoder for the common cases we actually encounter in
# markdown links: %20 (space), %28 / %29 (parens). Extendable.
urldecode() {
  printf '%s' "$1" | sed \
    -e 's/%20/ /g' \
    -e 's/%28/(/g' \
    -e 's/%29/)/g'
}

check_target() {
  # $1 = source markdown file
  # $2 = line number
  # $3 = raw target as it appeared in the (...) parens
  src=$1
  lineno=$2
  target=$3

  # Skip empty targets.
  [ -z "$target" ] && return 0

  # Skip external URLs.
  case "$target" in
    http://*|https://*|mailto:*|ftp://*|ftps://*|ssh://*|git://*)
      return 0
      ;;
  esac

  # Skip pure fragments (anchor-only links).
  case "$target" in
    \#*)
      return 0
      ;;
  esac

  # Skip cross-repository backticked paths embedded as links
  # (e.g. sum-chain:deploy/foo.yaml). These are documented not to
  # be local paths.
  case "$target" in
    sum-chain:*|sum-net:*|sum-store:*|sum-node:*|sum-types:*|sum-crypto:*)
      return 0
      ;;
  esac

  # Split off any ?query and #fragment.
  path=${target%%#*}
  path=${path%%\?*}
  [ -z "$path" ] && return 0

  # URL-decode common escapes.
  decoded=$(urldecode "$path")

  # Resolve relative to the source file's directory. Leading `/`
  # is treated as repo-root-relative.
  case "$decoded" in
    /*)
      resolved="${decoded#/}"
      ;;
    *)
      resolved="$(dirname "$src")/$decoded"
      ;;
  esac

  if [ ! -e "$resolved" ]; then
    printf 'BROKEN: %s:%s: %s (resolved to %s)\n' \
      "$src" "$lineno" "$target" "$resolved" >> "$FAILURES"
    return 0
  fi

  # Case-sensitivity guard. macOS's default HFS+ / APFS is
  # case-insensitive, so `[ -e MYFILE.MD ]` returns true even
  # when the tracked file is `myfile.md`. That would ship a
  # broken link to any Linux consumer of the docs. Force a
  # case-sensitive check via `find` (whose `-name` matches
  # exactly on all platforms).
  parent_dir=$(dirname "$resolved")
  base=$(basename "$resolved")
  if [ -d "$parent_dir" ]; then
    if [ -z "$(find "$parent_dir" -maxdepth 1 -name "$base" -print 2>/dev/null | head -1)" ]; then
      printf 'CASE MISMATCH: %s:%s: %s (resolved to %s, but no exact-case match under %s)\n' \
        "$src" "$lineno" "$target" "$resolved" "$parent_dir" >> "$FAILURES"
    fi
  fi
}

# Build the list of files to audit: README, CHANGELOG, and every
# .md under docs/, sorted for stable output.
FILES=$(
  {
    echo README.md
    echo CHANGELOG.md
    find docs -type f -name '*.md' | LC_ALL=C sort
  } | LC_ALL=C sort -u
)

for src in $FILES; do
  # Preprocess the source to a stream of "<line>:<content>" where
  # <content> has been stripped of:
  #   - fenced code blocks (whole lines between ``` markers), and
  #   - inline code spans (`...` on the same line).
  # Both harbor Markdown-syntax examples that look like real
  # links but aren't. The stripping is minimal — good enough for
  # SNIP docs, not a full CommonMark parser.
  awk '
    BEGIN { in_fence = 0 }
    {
      # Toggle fence state on ``` fences (start-of-line only).
      if ($0 ~ /^\`\`\`/) {
        in_fence = !in_fence
        print NR ":"
        next
      }
      if (in_fence) {
        print NR ":"
        next
      }
      # Strip inline code spans. GNU/BSD gsub does not support
      # non-greedy repetition, so we lean on the substitution
      # being applied left-to-right which happens to give the
      # correct behaviour for simple `code` spans.
      line = $0
      gsub(/\`[^\`]*\`/, "", line)
      print NR ":" line
    }
  ' "$src" | while IFS=: read -r lineno rest; do
    # Skip lines with no link candidates.
    case "$rest" in
      *']('*) ;;
      *) continue ;;
    esac
    echo "$rest" | grep -oE '\]\([^)]+\)' | while IFS= read -r fragment; do
      target=${fragment#\]\(}
      target=${target%\)}
      check_target "$src" "$lineno" "$target"
    done
  done
done

if [ -s "$FAILURES" ]; then
  cat "$FAILURES" >&2
  printf 'check-docs-links: one or more links unresolved\n' >&2
  exit 1
fi

printf 'check-docs-links: ok\n'
