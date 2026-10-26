#!/usr/bin/env bash
# Mechanical drift scan for docs/ral-wiki and docs/SPEC.md.
# Implements the Lint procedure from docs/ral-wiki/AGENTS.md; costs no tokens.
# Output: tab-separated work-list records; findings do not change exit status.
#   MAP       <page> <stamp> <n-commits> <covers_paths>
#   ANCHOR    <page> <stamp> <missing anchors, comma-separated>
#   UNVERIFIED_ANCHOR <page> <stamp> <anchor> <reason>
#   RELATED   <page> <stamp> <n new decisions since stamp>
#   SPEC      docs/SPEC.md <stamp> <n-commits over core/ ral/ exarch/>
#   UNKNOWN_HISTORY <page> <field> <stamp|none> <reason>
# Tool/prerequisite failures are nonzero; unknown history is not a zero count.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

wiki=docs/ral-wiki
command -v rg >/dev/null || { printf 'wiki-drift requires ripgrep (rg)\n' >&2; exit 1; }
head_commit=$(git rev-parse --verify HEAD)
shallow=$(git rev-parse --is-shallow-repository)

frontmatter_field() { # file key -> raw value (frontmatter only)
  awk -v key="$2" '
    NR==1 && $0!="---" { exit }
    NR>1 && $0=="---" { exit }
    $1==key":" { sub(/^[^:]*: */, ""); print; exit }
  ' "$1"
}

list_items() { # inline frontmatter list -> one item per line
  awk '
    { sub(/^[[:space:]]*\[/, ""); sub(/\][[:space:]]*$/, "") }
    {
      n=split($0, items, ",")
      for (i=1; i<=n; i++) {
        item=items[i]
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", item)
        if (item ~ /^".*"$/ || item ~ /^\047.*\047$/) item=substr(item, 2, length(item)-2)
        if (item != "") print item
      }
    }
  ' <<<"$1"
}

history_count() { # kind page field stamp paths... -> finding, if any
  local kind=$1 page=$2 field=$3 stamp=$4 commit reason='' n
  shift 4
  if [[ -z $stamp ]]; then
    reason='missing-stamp'
  elif ! commit=$(git rev-parse --verify --quiet --end-of-options "${stamp}^{commit}"); then
    reason=unresolvable-commit
  elif [[ $shallow == true && $commit != "$head_commit" ]]; then
    # A resolvable stamp alone does not prove the intervening history is complete.
    reason='shallow-history'
  fi
  if [[ -n $reason ]]; then
    printf 'UNKNOWN_HISTORY\t%s\t%s\t%s\t%s\n' "$page" "$field" "${stamp:-none}" "$reason"
    return
  fi
  n=$(git rev-list --count "$commit..HEAD" -- "$@")
  if [[ $n -gt 0 ]]; then
    printf '%s\t%s\t%s\t%s' "$kind" "$page" "$stamp" "$n"
    if [[ $kind == MAP ]]; then printf '\t%s' "$*"; fi
    printf '\n'
  fi
}

# Only production source trees, never vendor/ or generated build outputs.
anchor_sources=(
  core/src ral/src exarch/src synod/src vm-manager/src guest-net/src
  ral-daemon/src ral-initramfs/src vm-image/build-tools/src clipboard/src
)
for source in "${anchor_sources[@]}"; do
  [[ -d $source ]] || { printf 'missing anchor source: %s\n' "$source" >&2; exit 1; }
done

search() { # rg's no-match is data; a search error is fatal, even in a condition.
  local status=0
  rg "$@" || status=$?
  if [[ $status -gt 1 ]]; then exit "$status"; fi
  return "$status"
}

# Source-existence heuristic, not Rust name resolution. Only identifiers enter regexes.
anchor_present() { # 0 present, 1 missing, 2 unsupported
  local anchor=$1 owner leaf file files status
  if search -q -F -- "$anchor" "${anchor_sources[@]}"; then return 0; fi
  [[ $anchor =~ ^[a-zA-Z_][a-zA-Z_0-9]*(::[a-zA-Z_][a-zA-Z_0-9]*)*$ ]] || return 2
  [[ $anchor == *::* ]] || return 1
  owner=${anchor%::*}
  owner=${owner##*::}
  leaf=${anchor##*::}

  # A module-qualified item is declared in module.rs or module/{mod.rs,...}.
  files=$(rg -l -g '*.rs' "\b(struct|enum|trait|type|mod|const|static|fn)[[:space:]]+$leaf\b" "${anchor_sources[@]}") || {
    status=$?
    if [[ $status -gt 1 ]]; then exit "$status"; fi
  }
  while IFS= read -r file; do
    [[ -n $file ]] || continue
    if [[ $file == */"$owner.rs" || $file == */"$owner"/* ]]; then return 0; fi
  done <<<"$files"

  # Associated functions may have a multiline impl header; the file is the scope.
  files=$(rg -l -U -g '*.rs' "(?m)^[[:space:]]*impl\b[^{};]*\b$owner\b[^{};]*\{" "${anchor_sources[@]}") || {
    status=$?
    if [[ $status -gt 1 ]]; then exit "$status"; fi
  }
  while IFS= read -r file; do
    [[ -n $file ]] || continue
    if search -q "\bfn[[:space:]]+$leaf\b" "$file"; then return 0; fi
  done <<<"$files"
  return 1
}

# --- map/: stamp..HEAD over covers_paths ---
find "$wiki/map" -name '*.md' | sort | while IFS= read -r page; do
  stamp=$(frontmatter_field "$page" generated_at_commit)
  paths=$(frontmatter_field "$page" covers_paths)
  if [[ -z $paths ]]; then
    printf 'MAP\t%s\tUNSTAMPED\t-\t-\n' "$page"
    continue
  fi
  parr=()
  while IFS= read -r path; do parr+=("$path"); done < <(list_items "$paths")
  if [[ ${#parr[@]} -eq 0 ]]; then
    printf 'MAP\t%s\tUNSTAMPED\t-\t-\n' "$page"
    continue
  fi
  history_count MAP "$page" generated_at_commit "$stamp" "${parr[@]}"
done

# --- internals/, and active decisions/: anchors must still exist in the source ---
{ find "$wiki/internals" -name '*.md'; find "$wiki/decisions" -name '*.md'; } | sort | while IFS= read -r page; do
  [[ $page == */decisions/* && $(frontmatter_field "$page" status) != active ]] && continue
  stamp=$(frontmatter_field "$page" verified_at_commit)
  anchors=$(frontmatter_field "$page" anchors)
  [[ -z $anchors ]] && continue
  missing=()
  while IFS= read -r anchor; do
    status=0
    anchor_present "$anchor" || status=$?
    case $status in
      0) ;;
      1) missing+=("$anchor") ;;
      2) printf 'UNVERIFIED_ANCHOR\t%s\t%s\t%s\tunsupported-syntax\n' "$page" "${stamp:-UNSTAMPED}" "$anchor" ;;
      *) exit "$status" ;;
    esac
  done < <(list_items "$anchors")
  if [[ ${#missing[@]} -gt 0 ]]; then
    printf 'ANCHOR\t%s\t%s\t%s\n' "$page" "${stamp:-UNSTAMPED}" "$(IFS=,; echo "${missing[*]}")"
  fi
done

# --- related/: new decisions landed since verification ---
find "$wiki/related" -name '*.md' ! -name index.md | sort | while IFS= read -r page; do
  stamp=$(frontmatter_field "$page" verified_at_commit)
  history_count RELATED "$page" verified_at_commit "$stamp" "$wiki/decisions"
done

# --- SPEC.md: stamp lives in an HTML comment on line 1 ---
spec_stamp=$(awk 'NR==1 { if ($0 ~ /^<!-- verified_at_commit: [0-9a-f]+ -->$/) print $3; exit }' docs/SPEC.md)
history_count SPEC docs/SPEC.md verified_at_commit "$spec_stamp" core ral exarch
