#!/usr/bin/env bash
# Fails if the old name, Tidefell, is anywhere but where it belongs (ADR 0043): historic ADRs
# (0001-0042) and their titles in docs/SUMMARY.md, ADRs 0043 and 0044, the release notes of
# Tidefell's versions in docs/plan.md, and the README's "formerly Tidefell" line and licence
# grant.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

hits=$(git grep -n -I -i tidefell -- . \
  ':(exclude)docs/adr/00[0-3][0-9]-*.md' ':(exclude)docs/adr/004[0-4]-*.md' \
  ':(exclude)scripts/check-old-name.sh' || true)
hits=$(printf '%s\n' "$hits" | grep -v -E \
  'formerly Tidefell|under the name Tidefell|\(adr/00([0-3][0-9]|4[0-2])-' \
  || true)

# The release notes of Tidefell's versions: the plan's "- `v0." bullets, as written.
if [ -f docs/plan.md ]; then
  kept=$(awk '/^- `v0\./ { keep = 1 } keep && (/^- / && !/^- `v0\./ || /^#/ || /^$/) { keep = 0 }
              keep { print NR }' docs/plan.md | tr '\n' ' ')
  hits=$(printf '%s\n' "$hits" | awk -v kept=" $kept " -F: \
    '$1 == "docs/plan.md" && index(kept, " " $2 " ") { next } { print }')
fi

hits=$(printf '%s\n' "$hits" | sed '/^$/d')
if [ -n "$hits" ]; then
  echo "The old name is back (ADR 0043):"
  printf '%s\n' "$hits"
  exit 1
fi
echo "No stray Tidefell."
