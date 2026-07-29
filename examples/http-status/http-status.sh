#!/usr/bin/env bash
# Report the HTTP status code for each URL, concurrently, then tally by class.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    printf 'usage: %s URL...\n' "${0##*/}" >&2
    exit 2
fi
urls=("$@")

max_jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
case "$max_jobs" in '' | 0 | *[!0-9]*) max_jobs=4 ;; esac

dir="$(mktemp -d)"
trap 'rm -rf -- "$dir"' EXIT

probe() {
    local out="$1" url="$2" code rc=0
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -- "$url")" || rc=$?
    if [ -z "$code" ]; then
        printf 'no status from curl for %s (exit %d)\n' "$url" "$rc" >&2
        code=000
    fi
    printf '%s\t%s\0' "$code" "$url" > "$out"
}

i=0
while [ "$i" -lt "${#urls[@]}" ]; do
    probe "$dir/row.$i" "${urls[$i]}" &
    i=$((i + 1))
    if [ "$((i % max_jobs))" -eq 0 ]; then wait; fi
done
wait

i=0
while [ "$i" -lt "${#urls[@]}" ]; do
    if [ ! -s "$dir/row.$i" ]; then
        printf 'no row for %s\n' "${urls[$i]}" >&2
        exit 3
    fi
    cat -- "$dir/row.$i" >> "$dir/report"
    i=$((i + 1))
done

codes=()
dead=()
while IFS=$'\t' read -r -d '' code url; do
    printf '%s %s\n' "$code" "$url"
    codes+=("$code")
    if [ "$code" = 000 ]; then dead+=("$url"); fi
done < "$dir/report"

printf '%s\n' "${codes[@]}" |
    awk '{ n[$1 == "000" ? "unreachable" : substr($1, 1, 1) "xx"]++ }
         END { for (c in n) print c ": " n[c] }' | LC_ALL=C sort

if [ "${#dead[@]}" -gt 0 ]; then
    msg="${dead[0]}"
    i=1
    while [ "$i" -lt "${#dead[@]}" ]; do
        msg="$msg, ${dead[$i]}"
        i=$((i + 1))
    done
    printf 'unreachable: %s\n' "$msg" >&2
    exit 1
fi
