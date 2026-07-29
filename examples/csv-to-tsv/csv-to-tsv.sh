#!/usr/bin/env bash
# Convert a CSV file to TSV, columns in header-name order.
set -euo pipefail
export LC_ALL=C

if [ "$#" -ne 1 ]; then
    printf 'usage: %s FILE\n' "${0##*/}" >&2
    exit 2
fi

if [ -d "$1" ] || [ ! -r "$1" ]; then
    printf '%s: not a readable file\n' "$1" >&2
    exit 1
fi

out=$(mktemp) || exit 1
trap 'rm -f -- "$out"' EXIT

path="$1" awk '
function die(msg) { printf "%s: %s\n", ENVIRON["path"], msg > "/dev/stderr"; bad = 1; exit 1 }
function cut(rec, f,    i, c, n, cur, q, fresh) {
    n = 0; cur = ""; q = 0; fresh = 1
    for (i = 1; i <= length(rec); i++) {
        c = substr(rec, i, 1)
        if (q) {
            if (c != "\"") { cur = cur c }
            else if (substr(rec, i + 1, 1) == "\"") { cur = cur "\""; i++ }
            else { q = 0 }
        }
        else if (c == "\"" && fresh) { q = 1; fresh = 0 }
        else if (c == ",") { f[++n] = cur; cur = ""; fresh = 1 }
        else { cur = cur c; fresh = 0 }
    }
    f[++n] = cur
    open = q
    return n
}
function untsv(s) { return s ~ /[\t\n\r]/ }
{
    line = $0
    sub(/\r$/, "", line)
    rec = open ? rec "\n" line : line
    nf = cut(rec, f)
    if (open) { next }
    if (rec == "") { next }
    if (!seen) {
        seen = 1
        ncol = nf
        for (i = 1; i <= ncol; i++) { name[i] = f[i]; ord[i] = i }
        for (i = 2; i <= ncol; i++) {
            k = ord[i]
            for (j = i - 1; j >= 1 && name[ord[j]] > name[k]; j--) { ord[j + 1] = ord[j] }
            ord[j + 1] = k
        }
        head = ""
        for (i = 1; i <= ncol; i++) {
            if (i > 1 && name[ord[i]] == name[ord[i - 1]]) { die("duplicate header column \"" name[ord[i]] "\"") }
            if (untsv(name[ord[i]])) { die("a column name or field holds a tab, carriage return, or newline, which TSV cannot escape") }
            head = head (i > 1 ? "\t" : "") name[ord[i]]
        }
        next
    }
    body = ""
    for (i = 1; i <= ncol; i++) {
        cell = (ord[i] <= nf) ? f[ord[i]] : ""
        if (untsv(cell)) { die("a column name or field holds a tab, carriage return, or newline, which TSV cannot escape") }
        body = body (i > 1 ? "\t" : "") cell
    }
    if (!rows++) { print head }
    print body
}
END {
    if (bad) { exit 1 }
    if (open) { die("unterminated quoted field") }
    if (!rows) { die("no data rows to convert") }
}
' < "$1" > "$out"

cat -- "$out"
