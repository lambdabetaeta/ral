#!/usr/bin/env bash
# Rename every *.FROM in a directory to *.TO.
set -euo pipefail

dir="$1"; from="$2"; to="$3"

for f in "$dir"/*."$from"; do
    mv "$f" "${f%."$from"}.$to"
done
