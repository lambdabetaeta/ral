#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

npm install --prefix "$work" --no-audit --no-fund --package-lock=false \
    vscode-textmate@9.2.0 vscode-oniguruma@2.0.1
NODE_PATH="$work/node_modules" node "$root/tests/textmate.cjs"

cat > "$work/Cargo.toml" <<EOF
[package]
name = "ral-syntax-scope-tests"
version = "0.0.0"
edition = "2024"

[[bin]]
name = "syntect-scopes"
path = "$root/tests/syntect.rs"

[dependencies]
syntect = { version = "=5.3.0", default-features = false, features = ["parsing", "yaml-load", "regex-onig"] }
serde_json = "=1.0.145"
EOF
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/ral-syntax-scope-target}" \
    cargo run --quiet --manifest-path "$work/Cargo.toml" -- "$root"
