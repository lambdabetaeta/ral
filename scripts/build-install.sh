#!/bin/sh
# Build and install ral from source.
# Must be run from the repository root.
set -eu

if [ ! -f ral/Cargo.toml ]; then
    echo "Run this script from the repository root." >&2
    exit 1
fi

cargo install --force --path ral

ral_bin="${CARGO_HOME:-$HOME/.cargo}/bin/ral"
if [ "$(uname)" = "Darwin" ] && [ -f "$ral_bin" ]; then
    codesign -s - "$ral_bin"
fi
