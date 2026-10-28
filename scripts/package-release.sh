#!/bin/sh
# Package a ral or exarch binary with its applicable legal files.
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: package-release.sh BINARY ARCHIVE.{tar.gz,zip}" >&2
    exit 2
fi
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=$1
archive=$(CDPATH= cd -- "$(dirname -- "$2")" && pwd)/$(basename -- "$2")
name=$(basename -- "$binary")
case "$name" in
    ral|ral-*|ral.exe) editor=false ;;
    exarch|exarch-*|exarch.exe) editor=true ;;
    *) echo "unsupported release binary: $name (expected ral or exarch)" >&2; exit 2 ;;
esac
case "$archive" in
    *.tar.gz|*.zip) ;;
    *) echo "unsupported release archive: $archive (expected .tar.gz or .zip)" >&2; exit 2 ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT HUP INT TERM
cp "$binary" "$stage/$name"
cp "$repo_root/LICENSE-APACHE" "$repo_root/LICENSE-MIT" "$repo_root/NOTICE" "$stage/"
if [ "$editor" = true ]; then
    cp "$repo_root/vendor/edtui/LICENSE.md" "$stage/LICENSE-EDTUI"
fi
case "$archive" in
    *.tar.gz) (cd "$stage" && tar -czf "$archive" *) ;;
    *.zip)
        # PowerShell is available on the Windows release runner; zip is not.
        RELEASE_STAGE=$(cygpath -w "$stage") RELEASE_ARCHIVE=$(cygpath -w "$archive") \
            powershell.exe -NoProfile -Command \
            '$ErrorActionPreference = "Stop"; Compress-Archive -Path "$env:RELEASE_STAGE/*" -DestinationPath $env:RELEASE_ARCHIVE -Force'
        ;;
esac
