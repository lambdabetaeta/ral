---
status: proposed
---

# Provider configuration: auto-discovery, live models, a searchable picker

**Exarch declares almost nothing. A famous provider whose key is in the
environment auto-populates, and its model list is fetched live from the
provider; a hand-written config is needed only for *unusual* providers
(custom endpoints). The active model and request tuning are picker-driven
runtime state persisted to a local `.exarch`, not config.** The earlier
flag surface (`--provider`/`--model`/`--max-tokens`, one positional per
knob) and the genesis needs — OpenAI OAuth, and tuning knobs that "keep
changing" — all dissolve into: detect what the environment already offers,
ask the provider what models it has, and let the user pick.

The design is DRY by construction: provider knowledge (key env var,
default endpoint, adapter) lives once in `ProviderKind::info`; model lists
come from genai's `all_model_names`; wire protocols are genai's
`AdapterKind`. Nothing about a provider or its models is restated in a
config file or a shell script.

## Famous providers auto-populate

- **A key in the environment is the whole declaration.** For each known
  provider (`ProviderKind`), if its conventional key var is present
  (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
  `DEEPSEEK_API_KEY`), that provider is *available* — no config, no named
  profile, no restated var name.
- **Models come from the provider, not a file.** The model list is fetched
  via genai's `client.all_model_names(adapter)`, **lazily** when the picker
  first opens, **cached** to the XDG cache dir with a TTL, and with
  **manual entry** as the fallback when listing fails or the wanted model
  is not yet listed. A hardcoded model table would go stale; the provider
  is the source of truth.

## State lives per-project under XDG, not config

- **The active model and tuning are runtime state.** `state.json` holds the
  selected model and the tuning overrides (temperature, reasoning effort,
  verbosity, max-tokens). The pickers write it; startup loads it; it
  overrides until changed — "set it once, it sticks." It is *not*
  hand-authored config.
- **Per-project, keyed by a slug, under the XDG state home — reusing the
  session-log directory.** It lives at
  `$XDG_STATE_HOME/exarch/<project-slug(cwd)>/state.json`, the same
  per-project directory the session logs already use
  (`bootstrap::project_dir`, `project_slug`) — so the memory is per-project
  without scattering a dotfile into the working tree, and exarch's
  per-project artifacts are one findable place. Because it sits *outside*
  cwd, the sandboxed agent cannot reach it at all: the protection is
  structural, with no fs-deny-list entry needed (the agent writes only cwd
  and scratch). exarch's own process — the picker, not a tool call —
  writes it.

## Config is only for unusual providers

- **A hand-written `config.ral` describes custom endpoints, nothing else.**
  A self-hosted or non-famous endpoint is declared with its base URL, its
  key env var, and its wire protocol — `completions` (OpenAI v1),
  `responses` (OpenAI v2), or `anthropic` — which map 1:1 onto genai's
  `AdapterKind`. Famous providers need no entry.
- **Authority stays at the trusted home.** Because a redirected `endpoint`
  is an exfiltration channel (it points the agent's own traffic at an
  arbitrary server), unusual-provider config lives in
  `$XDG_CONFIG_HOME/exarch/config.ral` — never the working tree. It is
  evaluated through the shared
  [[decisions/260606_cacheless-module-loader|`evaluate_source`]] path under
  a no-authority [[design/grant|grant]] (deny `exec`/`net`/`fs`-write), so a
  config can only compute a value, never cause an effect
  ([[design/two-enforcers|two enforcers]]: the in-process gate suffices —
  with `exec` denied there is no route to the network).

## The pickers

- **`/model` is a searchable strip, not a floating modal.** The TUI is a
  flat stack of strips with no overlay layer; the picker honours that —
  a bordered pane over the prompt region listing the available models
  (live lists across every available provider), filtered as the user types,
  selected row in `Modifier::REVERSED`, Enter to switch, Esc to dismiss.
  Modal in behaviour (an early-return guard in `App::key`), flat in
  rendering. Search matters because a provider list can run to hundreds
  (OpenRouter).
- **A secondary tuning picker** sets temperature / effort / verbosity /
  max-tokens, reachable from the model picker or via `/tune`. Both pickers
  write `.exarch`; switching rebuilds the provider over the same transcript
  (a switch rebuilds the transport, not the history), and the live model
  shows in the per-frame status bar.

## Reasoning is never stripped

- **genai owns the per-adapter reasoning policy; exarch stays out of it.**
  The Anthropic adapter drops reasoning, the OpenAI *Responses* adapter
  ignores it on input, and the OpenAI *chat-completions* adapter echoes
  `reasoning_content` back — which **current DeepSeek/Kimi reasoners
  require**, or they 400 across a tool-use sequence. So the transcript
  keeps reasoning whole; a pre-strip would break DeepSeek/Kimi and buys
  nothing elsewhere. The cost (DeepSeek bills the re-sent reasoning) is
  accepted; Anthropic and OpenAI never receive it.

## Credentials

- **API keys auto-detected and scrubbed.** Each available provider's key is
  read from its conventional env var into an in-memory store, then those
  vars are scrubbed from the environment so no spawned child inherits a key.
  Resolution is eager (the scrub forecloses re-reading later).
- **OAuth is a future seam.** When it lands it is detected the same way —
  a cached token present for a provider makes it available — reachable as a
  `login` subcommand (headless) and inline from the model picker
  (interactive). No declaration; the `Credential` type hosts both arms.

## Implementation slices

1. **Auto-discovery + live models + the `/model` searchable picker +
   `.exarch` model persistence + startup load.** Replaces the
   provider/model flag surface; no config file needed in this slice.
2. **The tuning picker + tuning in `.exarch`.**
3. **Unusual-provider `config.ral`** (XDG, no-authority eval, the three
   protocols).
4. **OAuth** (token cache, refresh at genai's per-request `AuthResolver`,
   `login` subcommand + picker entry).

See [[design/exarch-architecture|exarch-architecture]],
[[map/exarch/provider|provider map]], [[map/exarch/session|session map]],
[[map/exarch/frontend|frontend map]], [[map/exarch/policy|policy map]],
[[design/grant|grant]], and
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].
