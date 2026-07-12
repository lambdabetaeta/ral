#!/usr/bin/env bash
# Mechanical drift scan for docs/ral-wiki and docs/SPEC.md.
# Implements the Lint procedure from docs/ral-wiki/AGENTS.md; costs no tokens.
# Output: one line per finding, tab-separated, suitable as a work-list.
#   MAP	<page>	<stamp>	<n-commits>	<covers_paths>
#   ANCHOR	<page>	<stamp>	<missing anchors, comma-separated>
#   RELATED	<page>	<stamp>	<n new decisions since stamp>
#   SPEC	docs/SPEC.md	<stamp|none>	<n-commits since stamp over core/ ral/ exarch/>
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

wiki=docs/ral-wiki

frontmatter_field() { # file key -> raw value (frontmatter only)
  awk -v key="$2" '
    NR==1 && $0!="---" { exit }
    NR>1 && $0=="---" { exit }
    $1==key":" { sub(/^[^:]*: */, ""); print; exit }
  ' "$1"
}

strip_list() { # "[a, b]" -> "a b"
  tr -d '[]' <<<"$1" | tr ',' ' ' | xargs echo
}

# --- map/: stamp..HEAD over covers_paths ---
find "$wiki/map" -name '*.md' | sort | while read -r page; do
  stamp=$(frontmatter_field "$page" generated_at_commit)
  paths=$(frontmatter_field "$page" covers_paths)
  if [[ -z $stamp || -z $paths ]]; then
    printf 'MAP\t%s\tUNSTAMPED\t-\t-\n' "$page"
    continue
  fi
  read -ra parr <<<"$(strip_list "$paths")"
  n=$(git rev-list --count "$stamp"..HEAD -- "${parr[@]}")
  [[ $n -gt 0 ]] && printf 'MAP\t%s\t%s\t%s\t%s\n' "$page" "$stamp" "$n" "${parr[*]}" || true
done

# --- internals/: anchors must still exist in the source ---
find "$wiki/internals" -name '*.md' | sort | while read -r page; do
  stamp=$(frontmatter_field "$page" verified_at_commit)
  anchors=$(frontmatter_field "$page" anchors)
  [[ -z $anchors ]] && continue
  missing=()
  for a in $(strip_list "$anchors"); do
    rg -q --fixed-strings "$a" core ral ral-sh exarch 2>/dev/null || missing+=("$a")
  done
  [[ ${#missing[@]} -gt 0 ]] && printf 'ANCHOR\t%s\t%s\t%s\n' "$page" "${stamp:-UNSTAMPED}" "$(IFS=,; echo "${missing[*]}")" || true
done

# --- related/: new decisions landed since verification ---
find "$wiki/related" -name '*.md' | sort | while read -r page; do
  stamp=$(frontmatter_field "$page" verified_at_commit)
  [[ -z $stamp ]] && continue
  n=$(git rev-list --count "$stamp"..HEAD -- "$wiki/decisions")
  [[ $n -gt 0 ]] && printf 'RELATED\t%s\t%s\t%s\n' "$page" "$stamp" "$n" || true
done

# --- SPEC.md: stamp lives in an HTML comment on line 1 ---
spec_stamp=$(sed -n '1s/^<!-- verified_at_commit: \([0-9a-f]*\) -->$/\1/p' docs/SPEC.md)
if [[ -z $spec_stamp ]]; then
  printf 'SPEC\tdocs/SPEC.md\tnone\t-\n'
else
  n=$(git rev-list --count "$spec_stamp"..HEAD -- core ral ral-sh exarch)
  [[ $n -gt 0 ]] && printf 'SPEC\tdocs/SPEC.md\t%s\t%s\n' "$spec_stamp" "$n" || true
fi
exit 0
