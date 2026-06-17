#!/usr/bin/env bash
# wiki-index.sh — build/refresh a project-local qmd search index for an LLM wiki.
#
# Drop this into any markdown wiki and run it. It builds a project-local qmd index
# under <root>/.qmd/ that covers only this wiki, so the qmd MCP server — which an
# editor launches with the repo root as its cwd — serves this index and no other.
# A search from the project can therefore only ever return this wiki's documents:
# isolation is structural, not a scoping convention. This is what makes one qmd
# install safe to share across many unrelated wikis.
#
# Mechanism: qmd resolves a project-local index by walking up from its working
# directory for .qmd/index.yaml (the way git finds .git). The MCP cwd is the repo
# root, so .qmd/ must sit at or above it — hence INDEX_ROOT defaults to the repo
# root. .qmd/ is a generated artefact and is git-ignored (this script adds the
# entry if missing). qmd has no file-watcher, so re-run after editing pages.
#
# The project-local index (`qmd init`, .qmd/index.yaml) is a real qmd feature but
# is not covered by qmd's own README/skill, which document the global
# `qmd collection add` path instead. We use the local index deliberately, for the
# isolation above.

set -euo pipefail

command -v qmd >/dev/null || { echo "qmd not found on PATH" >&2; exit 1; }

script_dir=$(cd "$(dirname "$0")" && pwd)
root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || echo "$script_dir")

# ── per-wiki configuration (each overridable via the matching env var) ────────
#   WIKI_DIR    markdown to index        — default: the whole repo ($root)
#   COLLECTION  collection name          — default: the repo's basename
#   INDEX_ROOT  directory holding .qmd/  — default: $root; must sit at or above
#               the cwd your editor launches qmd's MCP server from
# For a standalone wiki repo the defaults need no editing. Point WIKI_DIR at a
# subtree when the wiki is nested in a larger repo (here: ral's docs/ral-wiki).
WIKI_DIR=${WIKI_DIR:-"$root/docs/ral-wiki"}
COLLECTION=${COLLECTION:-ral-wiki}
INDEX_ROOT=${INDEX_ROOT:-"$root"}
# ──────────────────────────────────────────────────────────────────────────────

qmd_dir="$INDEX_ROOT/.qmd"
mkdir -p "$qmd_dir"

# Reuse the global qmd config's model hints so this index uses the same,
# already-downloaded models and fetches nothing. Absent a global config, qmd's
# own defaults apply (the embedder is downloaded once, then shared by all indices).
global_config="${XDG_CONFIG_HOME:-$HOME/.config}/qmd/index.yml"
[ -f "$global_config" ] || global_config="${XDG_CONFIG_HOME:-$HOME/.config}/qmd/index.yaml"
models_block=""
[ -f "$global_config" ] && models_block=$(awk '/^[^[:space:]]/{keep=($0 ~ /^models:/)} keep' "$global_config")

{
  printf 'collections:\n'
  printf '  %s:\n' "$COLLECTION"
  printf '    path: %s\n' "$WIKI_DIR"
  printf '    pattern: "**/*.md"\n'
  [ -n "$models_block" ] && printf '%s\n' "$models_block"
} > "$qmd_dir/index.yaml"

# Keep the generated index out of version control.
if git -C "$INDEX_ROOT" rev-parse --git-dir >/dev/null 2>&1 \
   && ! git -C "$INDEX_ROOT" check-ignore -q .qmd; then
  printf '\n# qmd project-local search index (generated; rebuild with %s)\n.qmd/\n' \
    "$(basename "$0")" >> "$INDEX_ROOT/.gitignore"
  echo "added .qmd/ to $INDEX_ROOT/.gitignore"
fi

# Build from INDEX_ROOT so qmd resolves the local .qmd/ index, not the global one.
cd "$INDEX_ROOT"
qmd update
qmd embed

echo "wiki index ready — $qmd_dir/index.sqlite ($COLLECTION ← $WIKI_DIR)"
