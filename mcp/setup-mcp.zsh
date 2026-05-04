#!/usr/bin/env zsh
# Install the ral MCP server and register it with Claude Code.
# Must be run from the repository root.
set -e

if [[ ! -d mcp ]]; then
    print -u2 "Run this script from the repository root."
    exit 1
fi

if ! command -v jq &>/dev/null; then
    print -u2 "jq is required but not installed."
    exit 1
fi

# Remove old cargo-installed ral-mcp if present.
cargo uninstall ral-mcp 2>/dev/null || true

# Install the Node.js MCP server globally.
(cd mcp && npm install && npm install -g . --force)

# ── Register in ~/.claude/settings.json ──────────────────────────────────────
CLAUDE_SETTINGS="$HOME/.claude/settings.json"
MCP_ENTRY='{"command":"ral-mcp","args":[]}'

if [[ ! -f "$CLAUDE_SETTINGS" ]]; then
    mkdir -p "$(dirname "$CLAUDE_SETTINGS")"
    echo '{"mcpServers":{}}' > "$CLAUDE_SETTINGS"
fi

tmp="${CLAUDE_SETTINGS}.tmp"
trap 'rm -f "$tmp"' EXIT

jq --argjson entry "$MCP_ENTRY" '.mcpServers.ral = $entry' \
    "$CLAUDE_SETTINGS" > "$tmp"
mv "$tmp" "$CLAUDE_SETTINGS"

echo "Registered ral MCP server in $CLAUDE_SETTINGS"
echo "Restart Claude Code for the MCP server to connect."
