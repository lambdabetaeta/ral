# exarch's config home: trusted because it is not the working tree

**exarch keeps its user-authored, trusted configuration in the XDG *config home*
— `$XDG_CONFIG_HOME/exarch/` — structurally outside the working tree the
sandboxed agent can write, and distinct from the per-project *state* and *cache*
homes.** Where a file lives is its trust level: the config home is the one place
the [[design/grant|grant]] cannot reach, so what sits there is exactly what the
agent must not be able to forge.

## The XDG split

Every exarch directory resolves through one helper, `bootstrap::xdg_app_dir(kind)`
(`exarch/src/bootstrap.rs`) — `$XDG_<kind>_HOME/exarch/` over the shared
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]]
resolver (`core/src/path/basedir.rs`, `XdgKind`). Three roles, three relationships
to trust:

- **config home** — `$XDG_CONFIG_HOME/exarch/` — hand-authored, trusted files. The
  one directory the operator writes and the agent never can.
- **state home** — `$XDG_STATE_HOME/exarch/<project-slug>/` via `bootstrap::project_dir`
  — the persisted model selection (`state.json`) and the per-run session logs,
  keyed by a slug of the launch `cwd` so a project's exarch state is one findable
  directory, never scattered into the working tree.
- **cache home** — `$XDG_CACHE_HOME/exarch/` — the model catalog cache, a
  refetchable snapshot with a TTL.

## What lives in the config home

- **`config.ral`** — the only declaration exarch cannot infer: a *custom*
  provider's base URL, key env var, and wire protocol
  ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
  A famous provider auto-populates from its env key and needs no entry; the file
  exists for the self-hosted or non-famous endpoint. Its authority argument is
  that a redirected endpoint *is* an exfiltration channel, so the declaration
  belongs at the trusted home, never in cwd.
- **`AGENTS.md`** — the operator's global agent instructions, the outermost link
  of the [[design/agents-md-injection|Workspace]] chain injected into the system
  prompt.

## The trust model

**The config home is trusted precisely because it is never the working tree.**
The sandboxed agent's [[design/grant|grant]] admits writes to `cwd` and the
per-session scratch dir alone, so it cannot reach `$XDG_CONFIG_HOME` at all — the
protection is *structural*, not a deny-list entry an oversight could omit
([[map/exarch/policy|policy]]).

- **A trusted location still gets a no-authority evaluation.** `config.ral` is
  source, so it is *evaluated*, not parsed (`exarch/src/config.rs`) — under a
  no-authority [[design/grant|grant]] (`Capabilities::deny_all`: `exec`, `net`,
  and `fs` all denied), in a throwaway shell. It can compute a value, cause
  no effect. The asymmetry is deliberate: even a file the agent cannot write is
  evaluated as if it might be hostile, because a redirected endpoint is an
  exfiltration channel and the in-process gate suffices to close it
  ([[design/two-enforcers|two enforcers]]: with `exec` denied there is no route
  to the network).

See also [[design/agents-md-injection|agents-md-injection]],
[[design/exarch-architecture|exarch-architecture]], [[design/grant|grant]],
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]],
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].
