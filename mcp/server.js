#!/usr/bin/env node
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { writeFileSync, unlinkSync, existsSync, readFileSync } from "node:fs";
import { tmpdir, platform } from "node:os";

const execAsync = promisify(execFile);
const __dirname = dirname(fileURLToPath(import.meta.url));

// ── Sandbox (Unix only) ───────────────────────────────────────────────────────

let SandboxManager = null;

if (platform() !== "win32") {
  try {
    ({ SandboxManager } = await import("@anthropic-ai/sandbox-runtime"));
    await SandboxManager.initialize({
      filesystem: {
        denyRead: [],
        allowWrite: [tmpdir()],
        denyWrite: [],
      },
      network: {
        allowedDomains: ["*"],
        deniedDomains: [],
      },
    });
  } catch {
    SandboxManager = null;
  }
}

// ── Shell quoting ─────────────────────────────────────────────────────────────

function shellQuote(s) {
  return "'" + String(s).replace(/'/g, "'\\''") + "'";
}

// ── Binary location ───────────────────────────────────────────────────────────

function findRal() {
  if (process.env.RAL_RUN) return process.env.RAL_RUN;
  for (const dir of (process.env.PATH || "").split(":")) {
    const candidate = join(dir, "ral");
    if (existsSync(candidate)) return candidate;
  }
  const release = resolve(__dirname, "../target/release/ral");
  if (existsSync(release)) return release;
  const debug = resolve(__dirname, "../target/debug/ral");
  if (existsSync(debug)) return debug;
  return "ral";
}

const RAL_RUN = findRal();

// ── Script runner ─────────────────────────────────────────────────────────────

async function runRal(script, args = [], timeout = 30_000) {
  const tmp = join(tmpdir(), `ral-mcp-${process.pid}-${Date.now()}.ral`);
  try {
    writeFileSync(tmp, script);

    let cmd, cmdArgs;
    if (SandboxManager) {
      const cmdStr = [RAL_RUN, ...args, tmp].map(shellQuote).join(" ");
      const wrapped = await SandboxManager.wrapWithSandbox(cmdStr);
      cmd = "sh";
      cmdArgs = ["-c", wrapped];
    } else {
      cmd = RAL_RUN;
      cmdArgs = [...args, tmp];
    }

    try {
      const { stdout, stderr } = await execAsync(cmd, cmdArgs, {
        timeout,
        maxBuffer: 10 * 1024 * 1024,
      });
      return { success: true, stdout: stdout.trimEnd(), stderr: stderr.trimEnd() };
    } catch (err) {
      return {
        success: false,
        stdout: (err.stdout || "").trimEnd(),
        stderr: (err.stderr || err.message).trimEnd(),
      };
    }
  } finally {
    try { unlinkSync(tmp); } catch {}
  }
}

// ── Tree renderer ─────────────────────────────────────────────────────────────

function findFailure(node) {
  for (const child of node.children || []) {
    const found = findFailure(child);
    if (found) return found;
  }
  if (node.status !== 0 && node.cmd) return node;
  return null;
}

function renderFailure(root) {
  const failed = findFailure(root);
  if (!failed) return root.stderr?.trimEnd() || "failed";
  const cmd = failed.cmd.replace(/.*\//, "");
  const args = (failed.args || []).join(" ");
  const label = args ? `${cmd} ${args}` : cmd;
  const lines = [`$ ${label}`];
  if (failed.stderr) lines.push(failed.stderr.trimEnd());
  if (failed.stdout) lines.push(failed.stdout.trimEnd());
  lines.push(`[exit ${failed.status}]`);
  return lines.join("\n");
}

// ── Server ────────────────────────────────────────────────────────────────────

const server = new McpServer(
  { name: "ral-mcp", version: "0.1.0" },
  {
    instructions:
      "ral shell MCP server. Before writing or running any ral script, " +
      "read the `ral://reference` resource — ral differs from bash in " +
      "important ways and the guide is short. Use `exec` to execute scripts, " +
      "`check` to validate syntax, and `ast` to inspect the parse tree.",
  }
);

server.tool(
  "exec",
  "Execute a ral script and return the execution trace. Each command is shown " +
  "with its output and exit status. Use this to run shell commands, process " +
  "data, and automate system tasks. On failure, the trace shows which steps " +
  "completed and what went wrong.",
  {
    script: z.string().describe("The ral script to execute"),
    timeout: z.number().optional().describe("Timeout in milliseconds (default 30000)"),
  },
  async ({ script, timeout }) => {
    const result = await runRal(script, ["--audit"], timeout || 30_000);
    let tree;
    try {
      tree = JSON.parse(result.stderr);
    } catch {
      return {
        content: [{ type: "text", text: result.stdout || result.stderr || "(no output)" }],
        isError: !result.success,
      };
    }
    if (result.success) {
      return {
        content: [{ type: "text", text: result.stdout.trimEnd() || "(no output)" }],
      };
    }
    return {
      content: [{ type: "text", text: renderFailure(tree) }],
      isError: true,
    };
  }
);

server.tool(
  "check",
  "Check the syntax of a ral script without executing it.",
  { script: z.string().describe("The ral script to check") },
  async ({ script }) => {
    const result = await runRal(script, ["--check"], 10_000);
    if (result.success) {
      return { content: [{ type: "text", text: "Syntax OK" }] };
    }
    return {
      content: [{ type: "text", text: result.stderr || "Syntax error" }],
      isError: true,
    };
  }
);

server.tool(
  "ast",
  "Parse a ral script and dump its AST. Useful for debugging parse issues or " +
  "understanding how ral parses a given expression.",
  { script: z.string().describe("The ral script to parse") },
  async ({ script }) => {
    const result = await runRal(script, ["--dump-ast"], 10_000);
    const output = result.stderr || result.stdout || "(no output)";
    return {
      content: [{ type: "text", text: output }],
      isError: !result.success,
    };
  }
);

// ── Resource ──────────────────────────────────────────────────────────────────

const GUIDE_PATH = resolve(__dirname, "../doc/RAL_GUIDE.md");
const RAL_REFERENCE = existsSync(GUIDE_PATH)
  ? readFileSync(GUIDE_PATH, "utf8")
  : "(reference not found)";

server.resource(
  "reference",
  "ral://reference",
  { mimeType: "text/markdown" },
  async () => ({
    contents: [{ uri: "ral://reference", mimeType: "text/markdown", text: RAL_REFERENCE }],
  })
);

// ── Entry point ───────────────────────────────────────────────────────────────

const transport = new StdioServerTransport();
await server.connect(transport);
