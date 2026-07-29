#!/usr/bin/env bash
# Reap named memory hogs on macOS and report what the reap recovered.
set -euo pipefail
export LC_ALL=C

if [ "$#" -eq 0 ]; then
    printf 'usage: mac-memory-reap.sh APP...\n' >&2
    exit 2
fi

free_pct() {
    memory_pressure |
        awk -F': *' '/^System-wide memory free percentage/ { gsub(/%/, "", $2); print $2 + 0 }'
}

census() { ps -Aco pid=,rss=,comm=; }

matching() {
    NEEDLE="$1" awk '
        {
            comm = $0
            sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+/, "", comm)
            if (tolower(comm) == tolower(ENVIRON["NEEDLE"])) { print $1, $2 }
        }
    '
}

alive_of() {
    PIDS="$1" awk '
        BEGIN {
            n = split(ENVIRON["PIDS"], want, " ")
            for (i = 1; i <= n; i++) { target[want[i]] = 1 }
        }
        ($1 in target) { n_alive += 1 }
        END { print n_alive + 0 }
    '
}

before="$(free_pct)"
printf 'free before: %s%%\n' "$before"

snapshot="$(census)"

apps=("$@")
pids_of=()
doomed=()
seen=""
total_kb=0
for app in "$@"; do
    procs=0
    kb=0
    pids=""
    while read -r pid rss; do
        [ -n "$pid" ] || continue
        pids="$pids $pid"
        procs=$((procs + 1))
        kb=$((kb + rss))
        case " $seen " in
            *" $pid "*) ;;
            *)
                seen="$seen $pid"
                doomed[${#doomed[@]}]="$pid"
                total_kb=$((total_kb + rss))
                ;;
        esac
    done <<<"$(printf '%s\n' "$snapshot" | matching "$app")"
    pids_of[${#pids_of[@]}]="$pids"
    printf '%s: %d procs, %d MB\n' "$app" "$procs" "$((kb / 1024))"
done
printf 'reaped %d MB total\n' "$((total_kb / 1024))"

if [ "${#doomed[@]}" -gt 0 ]; then
    kill -TERM -- "${doomed[@]}" || :
fi

sleep 1
snapshot="$(census)"

i=0
while [ "$i" -lt "${#apps[@]}" ]; do
    printf '%s: %s still alive\n' "${apps[$i]}" \
        "$(printf '%s\n' "$snapshot" | alive_of "${pids_of[$i]}")"
    i=$((i + 1))
done

printf 'free after: %s%%\n' "$(free_pct)"
