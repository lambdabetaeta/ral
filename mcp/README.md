# ral MCP Server

MCP (Model Context Protocol) server that exposes the ral shell language to AI assistants.

## Tools

| Tool | Purpose |
|------|---------|
| `run` | Execute a ral script; returns the execution tree as JSON |
| `check` | Check syntax without executing |
| `ast` | Dump the parsed AST (debug) |

## Resource

| URI | Content |
|-----|---------|
| `ral://reference` | Full ral language guide (`doc/RAL_GUIDE.md`) |

## Setup

```sh
cd mcp
npm install
```

Build ral first:

```sh
cd ..
cargo build --release
```

## Usage with Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "ral": {
      "command": "node",
      "args": ["/path/to/shell/mcp/server.js"]
    }
  }
}
```

## Usage with Claude Code

Add to `.claude/settings.json`:

```json
{
  "mcpServers": {
    "ral": {
      "command": "node",
      "args": ["/path/to/shell/mcp/server.js"]
    }
  }
}
```

## Environment

- `RAL_RUN` — path to the ral binary. Auto-detected in order: `PATH`, `../target/release/ral`, `../target/debug/ral`.
