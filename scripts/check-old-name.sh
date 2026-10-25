#!/usr/bin/env bash
# Fails if the old name, Tidefell, is anywhere but in the decision records that are history
# (ADR 0043): ADRs 0001-0044 and their titles in docs/SUMMARY.md.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

hits=$(git grep -n -I -i tidefell -- . \
  ':(exclude)docs/adr/00[0-3][0-9]-*.md' ':(exclude)docs/adr/004[0-4]-*.md' \
  ':(exclude)scripts/check-old-name.sh' || true)
hits=$(printf '%s\n' "$hits" | grep -v -E '\(adr/00([0-3][0-9]|4[0-4])-' || true)

hits=$(printf '%s\n' "$hits" | sed '/^$/d')
if [ -n "$hits" ]; then
  echo "The old name is back (ADR 0043):"
  printf '%s\n' "$hits"
  exit 1
fi
echo "No stray Tidefell."
