#!/usr/bin/env bash
# Build the ral/exarch wiki (docs/ral-wiki) into site/wiki/ with Quartz v5.
#
# Mirrors the "Build wiki" step in .github/workflows/pages.yml so you can preview
# the published wiki locally.  Quartz is its own Node project, so we clone it
# (pinned), drop in our config, and point its build at the wiki.
#
# Requires Node >= 22 and npm >= 10.9.2 (Quartz v5).  Pass --serve to live-preview
# instead of writing into site/wiki/.
set -euo pipefail

QUARTZ_REF="v5.0.0"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${TMPDIR:-/tmp}/ral-wiki-quartz"

rm -rf "$WORK"
git clone --depth 1 --branch "$QUARTZ_REF" https://github.com/jackyzha0/quartz.git "$WORK"
cp "$ROOT/scripts/wiki/quartz.config.yaml" "$WORK/quartz.config.yaml"

cd "$WORK"
npm ci
npx quartz plugin install

if [[ "${1:-}" == "--serve" ]]; then
  exec npx quartz build -d "$ROOT/docs/ral-wiki" --serve
fi

npx quartz build -d "$ROOT/docs/ral-wiki" -o "$ROOT/site/wiki"
echo "wiki built → $ROOT/site/wiki"
