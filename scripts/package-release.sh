#!/bin/sh
# Package a ral or exarch binary with its applicable legal files, generating
# its third-party notices for the TARGET and FEATURES it was built with.
# Needs just and cargo-about 0.9.3.
set -eu

if [ "$#" -lt 3 ]; then
    echo "usage: package-release.sh BINARY ARCHIVE.{tar.gz,zip} TARGET [FEATURE...]" >&2
    exit 2
fi
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=$1
archive=$(CDPATH= cd -- "$(dirname -- "$2")" && pwd)/$(basename -- "$2")
target=$3
shift 3
name=$(basename -- "$binary")
case "$name" in
    ral|ral-*|ral.exe) package=ral notices=THIRDPARTY-RAL ;;
    exarch|exarch-*|exarch.exe) package=exarch notices=THIRDPARTY-EXARCH ;;
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
if [ "$package" = exarch ]; then
    cp "$repo_root/vendor/edtui/LICENSE.md" "$stage/LICENSE-EDTUI"
fi
just --justfile "$repo_root/justfile" third-party "$package" "$target" "$stage/$notices" "$@"
case "$archive" in
    *.tar.gz) (cd "$stage" && tar -czf "$archive" *) ;;
    *.zip)
        # PowerShell is available on the Windows release runner; zip is not.
        RELEASE_STAGE=$(cygpath -w "$stage") RELEASE_ARCHIVE=$(cygpath -w "$archive") \
            powershell.exe -NoProfile -Command \
            '$ErrorActionPreference = "Stop"; Compress-Archive -Path "$env:RELEASE_STAGE/*" -DestinationPath $env:RELEASE_ARCHIVE -Force'
        ;;
esac
