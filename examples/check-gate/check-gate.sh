#!/usr/bin/env bash
# Run every check, summarise which failed, and exit 1 if any did.
set -uo pipefail

rc=0
report=""

run_check() {
    local name="$1"; shift
    local out
    out=$("$@" 2>&1)
    if [[ $? -eq 0 ]]; then
        echo "ok    $name"
    else
        echo "FAIL  $name"
        report+="  $name:"$'\n'"$out"$'\n'
        rc=1
    fi
}

check_shell() { local f; for f in scripts/*.sh; do [[ -e $f ]] && sh -n -- "$f" || return; done; }
check_json()  { local f; for f in data/*.json; do [[ -e $f ]] && jq empty -- "$f" || return; done; }

run_check 'shell syntax' check_shell
run_check 'json'         check_json
run_check 'whitespace'   git diff --check

printf '%s' "$report"
exit "$rc"
