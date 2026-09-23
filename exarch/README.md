# exarch

A tiny coding agent in the spirit of swe-bench's mini-agent. It loops a
chosen LLM provider against a single tool — `ral` — that evaluates a
ral source string in process against a persistent `Shell`.

Each command the model emits is evaluated under a profile's
`Capabilities` pushed onto ral's capability stack, so ral's in-language
capability mechanism scopes file and exec access.  Six profiles ship
in the binary (`dangerous`, `reasonable`, `edit-only`, `read-only`,
`minimal`, `confined`); see [`PROFILES.md`](PROFILES.md) for what each admits and
when to use it.  `reasonable` is the default.

The name is the role: in Byzantine usage, an *exarch* was a viceroy
who acted on behalf of a distant sovereign within a bounded province.
Here the sovereign is the LLM and the province is the `grant`.

## Run

```
ANTHROPIC_API_KEY=…  cargo run -p exarch
```

A REPL prompt (`▸`) opens. Each line is a new user message in the same
conversation.  Every session is recorded under
`$XDG_STATE_HOME/exarch/<project>/<run>/sessions/<id>/`, whose directory is
printed at exit: `record.jsonl` is the full record, including unabridged
stdout and stderr from every command (the TUI itself shows a head/tail
digest for noisy commands), `record.log` a readable rendering of it, and
`user.log` the screen's transcript.  Type `/quit` (or send EOF) to
exit.

Run one headless exchange with `--prompt`; it implies `--headless`, writes the
final reply to standard output, and exits. A file can still seed the interactive
session, or run headlessly when combined with `--headless`:

```
cargo run -p exarch -- --prompt "list the rust files"
cargo run -p exarch -- --file task.md
cargo run -p exarch -- --headless --file task.md
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
| `gemini`       | `GEMINI_API_KEY`     | `gemini-2.5-pro`          |
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
[`examples/config.ral`](examples/config.ral) for the format.

A **signed-in ChatGPT account** is the one credential not read from the
environment: it authorises over OAuth. `chatgpt` is a service like any other
above, but unlike them it can own **several accounts** — a login email carries
a personal account and one per workspace, and OpenAI issues each its own id.
Sign in as many as you like with `exarch login`; each is separately selectable
in the `/model` picker, listed by `exarch accounts`, and named by its email,
qualified by its workspace when two would otherwise read alike. Every other
service owns exactly one account and goes by its own name, which is why
`--provider deepseek` names a credential unambiguously and `--provider
alex@example.com` may not: if two accounts answer to a name, exarch refuses it
and prints both rather than choosing for you.

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
  exec: [git: 'allow', cargo: 'allow'],
  fs:   [read:  ['cwd:', 'tempdir:'],
         write: ['cwd:', 'tempdir:']],
  net:  false,
  shell: [chdir: true],
]
```

The Exarch process itself is not sandboxed — it still needs HTTPS for
the model API.  Each tool call is evaluated as a top-level turn under
the profile's caps.  The interpreter stays in process and checks every
effect it performs itself; each external program it spawns is confined
by the platform sandbox (Seatbelt on macOS, bwrap with Landlock and
seccomp on Linux, a per-command AppContainer LowBox token on Windows),
which enforces the same file, network and, on macOS and Linux,
executable restrictions on whatever that program does after spawn.
On Windows each granted path carries an ACE for a capability SID
derived from that path, which the child's token holds only when the
grant admits it, and `net: false` withholds the network capability
SIDs so a denied command cannot open a socket at all.

Treat Exarch as a development tool, not a hardened jail.
