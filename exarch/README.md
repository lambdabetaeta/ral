# exarch

A tiny coding agent in the spirit of swe-bench's mini-agent. It loops a
chosen LLM provider against a single tool — `ral` — that evaluates a
ral source string in process against a persistent `Shell`.

Each command the model emits is evaluated under a profile's
`Capabilities` pushed onto ral's capability stack, so ral's in-language
capability mechanism scopes file and exec access.  Five profiles ship
in the binary (`dangerous`, `reasonable`, `read-only`, `minimal`,
`confined`); see [`PROFILES.md`](PROFILES.md) for what each admits and
when to use it.  `reasonable` is the default.

The name is the role: in Byzantine usage, an *exarch* was a viceroy
who acted on behalf of a distant sovereign within a bounded province.
Here the sovereign is the LLM and the province is the `grant`.

## Run

```
ANTHROPIC_API_KEY=…  cargo run -p exarch
```

A REPL prompt (`▸`) opens. Each line is a new user message in the same
conversation; the provider keeps history in memory only.  A
`session.log` is always written under `$EXARCH_SCRATCH` and its path is
printed at exit; it captures the full transcript including
unabridged stdout and stderr from every command (the TUI itself shows
a head/tail digest for noisy commands).  Type `/quit` (or send EOF) to
exit.

Seed the conversation with a prompt from a string or a file; the REPL
opens after the seed turn finishes:

```
cargo run -p exarch -- --prompt "list the rust files"
cargo run -p exarch -- --file task.md
```

## Providers and models

Every provider whose conventional key variable is set in the environment
is auto-discovered and available; no flag names a provider. The keys are
read into memory and scrubbed from the environment at startup so no
spawned child inherits them.

| provider     | key env var          | default model               |
|--------------|----------------------|-----------------------------|
| `anthropic`  | `ANTHROPIC_API_KEY`  | `claude-opus-4`             |
| `openai`     | `OPENAI_API_KEY`     | `gpt-5.5`                   |
| `openrouter` | `OPENROUTER_API_KEY` | `anthropic/claude-opus-4`   |
| `deepseek`   | `DEEPSEEK_API_KEY`   | `deepseek-chat`             |

Type `/model` in the REPL for a searchable picker over every available
provider's live model list (fetched from the provider and cached); the
selection persists per project under `$XDG_STATE_HOME/exarch/<project>/`
(beside that project's session logs) and is restored on the next start.
Because it lives outside the working directory, the sandboxed agent cannot
reach it. For headless or scripted runs, `--model <name>` sets the initial
model (its provider is resolved as the available provider whose list
contains it). With no `--model` and no saved selection, the first available
provider's default model is used.

## Layout

- `build.rs` — bakes the ral prelude into `OUT_DIR` (port of `ral/build.rs`).
- `src/shell_eval.rs` — prelude `OnceLock`s and the in-process
  `run_shell` that evaluates each tool call as a top-level turn
  (`evaluator::eval_top_level`) under the profile's capabilities
  (`Shell::with_capabilities`), captures stdout/stderr into in-memory
  buffers, and reuses one `Shell` across calls.
- `src/api.rs` — `Provider` trait with two implementations: `Anthropic`
  (Messages API) and `ChatCompletions` (one struct, two constructors:
  `::openai` and `::openrouter`).
- `src/main.rs` — argv, provider selection, persistent `Shell` boot, the
  loop, and the system prompt that teaches the model ral idioms.
- `src/ui.rs` — truecolor neon transcript: banner, turn separators,
  tool-call frames, exit colouring.

## Wire shape

`Provider::step(Step) -> StepOut` is provider-agnostic.

- **Anthropic** sends `tool_use` content blocks; results return as
  `tool_result` blocks inside a `user` message. Done when
  `stop_reason != "tool_use"`.
- **Chat Completions** sends `tool_calls` on the assistant message with
  `arguments` as a JSON-encoded string; results return as dedicated
  `role: "tool"` messages keyed by `tool_call_id`. Done when
  `finish_reason != "tool_calls"`.

The conversation is stateless on the wire — the exarch replays the full
history each turn — but the in-process `Shell` persists, so cwd, env,
and `let`-bound names survive across tool calls.

Each tool call is a *top-level turn* in the ral sense (SPEC §4.3.1):
`let`, `cd`, `env-set`, module loads, and the recorded last status
persist into the next call (the profile's `Capabilities` are pushed
onto the capability stack by `with_capabilities`, not by an enclosing
source-level `grant { … }` block).  A `grant { … }` block the model
writes itself, by contrast, still discards its body's bindings at the
closing brace, as for any other block.

## Sandbox

The boundary is the active profile's `Capabilities`, pushed onto the
capability stack for every tool call.  A profile is shaped exactly
like the argument of `grant [...]`:

```
[
  exec: [git: [], cargo: [], …],
  fs:   [read:  ['<cwd>', '/tmp'],
         write: ['<cwd>', '/tmp']],
  net:  false,
  shell: [chdir: true],
]
```

The Exarch process itself is not sandboxed — it still needs HTTPS for
the model API.  Each tool call is evaluated as a top-level turn under
the profile's caps; when those caps include filesystem or network
restrictions, ral re-execs a child process under the platform sandbox
(Seatbelt on macOS, bwrap on Linux) and the child evaluates the
computation there, returning the post-run program state to the parent.
Exec permissions are checked in ral before spawning; file/network
permissions are also enforced by the OS sandbox where supported.

Treat Exarch as a development tool, not a hardened jail.
