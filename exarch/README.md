# exarch

A tiny coding agent in the spirit of swe-bench's mini-agent. It loops a
chosen LLM provider against a single tool — `shell` — that evaluates a
ral source string in process against a persistent `Shell`.

Each command the model emits is wrapped in a `grant` block so ral's
in-language capability mechanism scopes file and exec access to the
current working directory and `/tmp`. Outbound network is denied.

The name is the role: in Byzantine usage, an *exarch* was a viceroy
who acted on behalf of a distant sovereign within a bounded province.
Here the sovereign is the LLM and the province is the `grant`.

## Docker

No checkout, no Rust toolchain.  Set at least one provider key and run
from the directory you want exarch to work in:

```
docker run --rm -it -e ANTHROPIC_API_KEY -v "$PWD:/work" ghcr.io/lambdabetaeta/exarch-box
```

Pass `-e OPENAI_API_KEY` or `-e OPENROUTER_API_KEY` instead to switch
providers.  Set `EXARCH_PROVIDER` and `EXARCH_MODEL` to pick a non-default
model:

```
docker run --rm -it \
    -e ANTHROPIC_API_KEY \
    -e EXARCH_PROVIDER=anthropic \
    -e EXARCH_MODEL=claude-sonnet-4-6 \
    -v "$PWD:/work" ghcr.io/lambdabetaeta/exarch-box
```

Append `--prompt` or `--file` to seed the first turn without opening the REPL:

```
docker run --rm -it -e ANTHROPIC_API_KEY -v "$PWD:/work" \
    ghcr.io/lambdabetaeta/exarch-box --prompt "describe this repo"
```

Remove the image when done:

```
docker rmi ghcr.io/lambdabetaeta/exarch-box
```

From this repo (builds from the latest release binary):

```
docker compose -f exarch/docker/compose.yaml run --rm --build exarch-box
```

See [`exarch/docker/`](docker/) for pinning a version, overriding the
workspace mount, and the local-build variant.

## Run

```
ANTHROPIC_API_KEY=…  cargo run -p exarch
```

A REPL prompt (`▸`) opens. Each line is a new user message in the same
conversation; the provider keeps history in memory only — nothing is
written to disk. Type `/quit` (or send EOF) to exit.

Seed the conversation with a prompt from a string or a file; the REPL
opens after the seed turn finishes:

```
cargo run -p exarch -- --prompt "list the rust files"
cargo run -p exarch -- --file task.md
```

Switch providers with `--provider` and override the model with
`--model`:

| `--provider` | key env var          | default model                  |
|--------------|----------------------|--------------------------------|
| `anthropic`  | `ANTHROPIC_API_KEY`  | `claude-opus-4-7`              |
| `openai`     | `OPENAI_API_KEY`     | `gpt-5.5`                      |
| `openrouter` | `OPENROUTER_API_KEY` | `anthropic/claude-opus-4.7`    |

## Layout

- `build.rs` — bakes the ral prelude into `OUT_DIR` (port of `ral/build.rs`).
- `src/eval.rs` — prelude `OnceLock`s, env seeding, `wrap_grant`, the
  in-process `run_shell` that captures stdout/stderr into in-memory
  buffers and reuses one `Shell` across calls.
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

## Sandbox

The boundary is the `grant` block emitted around every command:

```
grant [
  exec: [git: [], cargo: [], …],
  fs:   [read:  ['<cwd>', '/tmp'],
         write: ['<cwd>', '/tmp']],
  net:  false,
  shell: [chdir: true],
] { <model command> }
```

Process-level sandboxing (`sandbox::early_init`) is *not* applied — that
would also block the exarch's HTTPS calls. The grant block is the only
gate. Treat the exarch as a development tool, not a hardened jail.
