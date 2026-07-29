#!/usr/bin/env bash
# Parse a key=value config file, emit JSON to stdout.
set -euo pipefail

[[ $# -eq 1 ]] || { printf 'usage: kv-to-json.sh <file>\n' >&2; exit 1; }
path="$1"
[[ -f "$path" ]] || { printf 'kv-to-json.sh: %s: not a file\n' "$path" >&2; exit 1; }

pairs=()
while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" == *=* ]] || continue
    key="${line%%=*}"
    val="${line#*=}"
    key="${key#"${key%%[![:space:]]*}"}"
    key="${key%"${key##*[![:space:]]}"}"
    val="${val#"${val%%[![:space:]]*}"}"
    val="${val%"${val##*[![:space:]]}"}"
    [[ -n "$key" && "$key" != \#* ]] || continue
    if [[ "$val" == '"'*'"' || "$val" == "'"*"'" ]]; then
        val="${val:1:${#val}-2}"
    fi
    pairs+=("$key" "$val")
done < "$path"

# `--arg`/$ARGS.named keeps the first of a repeated key; `add` keeps the last.
jq -nSc '[range(0; ($ARGS.positional | length); 2)
          | {($ARGS.positional[.]): $ARGS.positional[. + 1]}] | add // {}' \
   --args ${pairs[@]+"${pairs[@]}"}
