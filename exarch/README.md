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

| provider       | key env var          | default model             |
|----------------|----------------------|---------------------------|
| `anthropic`    | `ANTHROPIC_API_KEY`  | `claude-opus-4`           |
| `openai`       | `OPENAI_API_KEY`     | `gpt-5.5`                 |
| `openrouter`   | `OPENROUTER_API_KEY` | `anthropic/claude-opus-4` |
| `deepseek`     | `DEEPSEEK_API_KEY`   | `deepseek-chat`           |
| `opencode-zen` | `OPENCODE_API_KEY`   | `glm-5.1`                 |
| `opencode-go`  | `OPENCODE_API_KEY`   | `glm-5.2`                 |
| `xai`          | `XAI_API_KEY`        | `grok-4.3`                |
| `qwen`         | `DASHSCOPE_API_KEY`  | `qwen3.6-plus`            |

`opencode-zen` and `opencode-go` share one `OPENCODE_API_KEY` — one account
key, two endpoints — so setting it makes both available. A key value with a
stray control character (e.g. a pasted newline) is rejected as malformed, but
still scrubbed.

A **custom or self-hosted endpoint** exarch has no built-in knowledge of is
declared in `$XDG_CONFIG_HOME/exarch/config.ral` with its base URL, the *name*
of the env var holding its key, and its wire protocol; the key itself still
comes from the environment and is scrubbed like a famous provider's. See
[`examples/config.ral`](examples/config.ral) for the format. A signed-in
ChatGPT account is the one credential not read from the environment: it
authorises over OAuth and appears as its own selectable provider.

Type `/model` in the REPL for a searchable picker over every available
provider's live model list (fetched from the provider and cached); the
selection persists per project under `$XDG_STATE_HOME/exarch/<project>/`
(beside that project's session logs) and is restored on the next start.
Because it lives outside the working directory, the sandboxed agent cannot
reach it. For headless or scripted runs, `--model <name>` sets the initial
model (its provider is resolved as the available provider whose list
contains it). With no `--model` and no saved selection, the first available
provider's default model is used.

Every path above goes through XDG with Linux-shaped defaults, even on
Windows: config lives under `%USERPROFILE%\.config\exarch`, state (session
logs, the model-picker selection, the OAuth token store) under
`%USERPROFILE%\.local\state\exarch` — not `%APPDATA%`. This is deliberate,
not an oversight: it keeps one config/state layout across every platform
rather than a Windows-specific Known Folders migration with no functional
payoff. Set `XDG_CONFIG_HOME`/`XDG_STATE_HOME` to relocate either.

## Sandbox

The boundary is the active profile's `Capabilities`, pushed onto the
capability stack for every tool call.  A profile is shaped exactly
like the argument of `grant [...]`:

```
[
  exec: [git: 'allow', cargo: 'allow', …],
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
(Seatbelt on macOS, bwrap on Linux, a per-command AppContainer LowBox
token on Windows) and the child evaluates the computation there,
returning the post-run program state to the parent. Exec permissions
are checked in ral before spawning; file/network permissions are also
enforced by the OS sandbox where supported. On Windows the fs
allow-list is enforced by ACEs stamped for the AppContainer's SID on
the granted prefixes, and `net: false` withholds the network
capability SIDs so a denied command cannot open a socket at all.

Treat Exarch as a development tool, not a hardened jail.
