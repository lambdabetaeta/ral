---
generated_at_commit: e28f2054
generated_at_date: 2026-06-16
covers_paths: [exarch/src/main.rs, exarch/src/cli.rs, exarch/src/bootstrap.rs, exarch/src/prompt.rs, exarch/data/system.md, exarch/data/ral.md, exarch/data/script-style.md]
---

# Map: exarch

exarch is a small LLM coding agent — a separate workspace binary that
**embeds** [[map/core|ral-core]] (`ral-core = { path = "../core" }`). A model
(Anthropic / OpenAI / OpenRouter / DeepSeek) is given a `shell` tool, and each
tool call is evaluated as a ral top-level turn against a persistent in-process
`Shell`, under capabilities the user chose. It ships as one executable
([[invariants/single-binary|single-binary]]); the same binary re-execs itself as the
[[map/core/capabilities|OS sandbox child]].

`main.rs` is the front door: it parses the CLI (`cli.rs`), composes the
capability lattice (`policy.rs`, `policy/`), assembles the layered system prompt
(`prompt.rs`: basic rules, live grant/host, ral language card, script-style
guide), builds a `Session` + `Provider`, and hands off to one frontend.
`bootstrap.rs` holds the once-per-process pieces: `sandbox_dispatch_or_continue`
(the re-exec child mode), `boot_shell` (the exarch-ready session-shell
constructor: clear stale ral interrupt, install ral handlers, re-chain exarch
cancel, call core's [[map/repl/startup|`ral_core::host::boot_shell`]], then layer
exarch's host builtins and source the `agent.ral` library), the disposable
per-session `Scratch` directory exposed as `$EXARCH_SCRATCH`, and `log_run_dir`
— the durable per-run [[map/exarch/frontend|session-log]] directory
at `$XDG_STATE_HOME/exarch/<project>/<run>/`, keyed by a slug of the project cwd
(`project_slug`), so logs survive an abnormal exit.

## Subsystems

- [[map/exarch/session|session]] — the turn loop: provider round-trips, tool dispatch
  with prompt-queue steering, auto-compaction, the nudge-retry policy, sub-agent fork.
- [[map/exarch/provider|provider]] — LLM transport over genai: streaming, the retry
  driver, prompt caching, usage and dollar accounting.
- [[map/exarch/shell-eval|shell-eval]] — one tool call as a ral top-level turn under a
  pushed capabilities frame; the streaming digest and the surface host sink.
- [[map/exarch/policy|policy]] — capability composition (base ∨ extend ⊓ restrict) and
  the bake-in profiles; the boundary *is* ral's [[design/grant|grant]].
- [[map/exarch/tools|tools]] — the tool registry: `shell`, `agent`, `fff`; `agent` forks are joined at steering boundaries.
- [[map/exarch/builtins|builtins]] — the resident host atoms: the hash-addressed edit
  primitives and the `agent.ral` helpers ([[design/hash-addressed-editing|why]]).
- [[map/exarch/frontend|frontend]] — the agent/UI boundary (event bus, session log) and
  the two frontends, the inline TUI and headless.

## Sandbox

exarch does not invent its own sandbox. Each turn is evaluated under a
**profile's capabilities pushed onto ral's capability stack** —
`Shell::with_capabilities(caps, |s| eval_top_level(…, s))` — so the safety
boundary *is* ral's [[design/grant|grant]] mechanism — authority attenuated by
intersection. There is no source-level `grant { … }` the model could escape;
the frame is installed by the host. Profiles ship as `.exarch.ral` files in
`exarch/data/` (`dangerous`, `reasonable`, `read-only`, `minimal`, `confined`);
see `exarch/PROFILES.md` and [[map/exarch/policy|policy]].

## Where to look

- `exarch/data/{system.md, ral.md, script-style.md, grant-legend.md, agent.ral}` —
  the persona rules, ral reference, reusable-script guide, grant legend, and the
  embedded agent helper library.
- `exarch/README.md`, `exarch/PROFILES.md` — human docs.

[[map/repl|repl]] is the sibling `ral` binary over the same engine. The old
scoped agent-batching slice of backgroundable tool work is superseded by
[[decisions/260616_tool-boundary-steering|tool-boundary-steering]]; true
backgroundable tools remain a separate future direction.
