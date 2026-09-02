#!/usr/bin/env bash
# Assemble site/ for local preview or upload.
# Copies assets/logo.svg into site/ when present (e.g. after PR #19 lands).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SITE="$ROOT/site"

if [[ ! -f "$SITE/index.html" ]]; then
  echo "error: $SITE/index.html is missing" >&2
  exit 1
fi

if [[ -f "$ROOT/assets/logo.svg" ]]; then
  cp "$ROOT/assets/logo.svg" "$SITE/logo.svg"
  echo "site: included assets/logo.svg"
else
  rm -f "$SITE/logo.svg"
  echo "site: no assets/logo.svg (image omitted)"
fi
