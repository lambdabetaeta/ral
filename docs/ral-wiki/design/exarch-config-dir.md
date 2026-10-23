# exarch's config home: trusted because it is not the working tree

**exarch keeps its user-authored, trusted configuration in the XDG *config home*
— `$XDG_CONFIG_HOME/exarch/` — structurally outside the working tree the
sandboxed agent can write, and distinct from the per-project *state* and *cache*
homes.** Where a file lives is its trust level: the config home is the one place
no [[design/grant|grant]] makes writable, so what sits there is exactly what the
agent must not be able to forge. **The config home holds only what the user
writes**: everything exarch keeps for itself lives in the state, cache or data
homes, so a config folder linked into a dotfiles repository collects no key,
token or command history.

## The XDG split

Every exarch directory resolves through `app::App` (`exarch/src/app.rs`), with
`app::EXARCH` supplying the `exarch` component — `$XDG_<kind>_HOME/exarch/`,
spelled by core's `path::app_dir`, the convention ral's own directories share, over
the shared
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]]
resolver (`core/src/path/basedir.rs`, `XdgKind`). `App::xdg_dir(kind)` is that
spelling as a name, for a deny or a skill root; `App::own(kind)` is the directory
*held* (`ral_core::own::OwnDir`), and everything exarch writes goes through it
([[decisions/261022_own-files|own-files]]). Where no `HOME` is bound, the
account's own home from the user database stands in, and past that
`OwnError::NoBase` says what to set: no program file falls back to the temp
directory. Three roles, three relationships to trust:

- **config home** — `$XDG_CONFIG_HOME/exarch/` — hand-authored, trusted files, and
  nothing else. The one directory the operator writes and the agent never can.
- **state home** — `$XDG_STATE_HOME/exarch/` — what exarch keeps: beneath
  `<project-slug>/`, via `App::project(cwd)`, the persisted choice of provider,
  model and via (`state.json`) and the per-run session logs (`App::new_run`),
  keyed by a slug of the launch `cwd` so a project's exarch state is one findable
  directory, never scattered into the working tree; beside it, the credentials
  exarch holds (`oauth.json`, a ChatGPT login's tokens, and `keys.json`, the
  keychain fallback, both owner-only) and the network audit ledger.
- **cache home** — `$XDG_CACHE_HOME/exarch/` — the model catalog cache, a
  refetchable snapshot with a TTL, and `scratch/`, the disposable directory of each
  live session (a grant names it `` `scratch . ``).

## What lives in the config home

- **`config.ral`** — the only declaration exarch cannot infer: a *custom*
  provider's base URL, key env var, and wire protocol
  ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
  A famous provider auto-populates from its env key and needs no entry; the file
  exists for the self-hosted or non-famous endpoint. Its authority argument is
  that a redirected endpoint *is* an exfiltration channel, so the declaration
  belongs at the trusted home, never in cwd. It is a `UserFile`, a file the user
  writes at a place exarch chooses: read with its links followed, and replaced
  (by synod's accounts screen, which writes declarations back) beside the file a
  link reaches, so a dotfile link survives and the repository behind it keeps the
  change.
- **`AGENTS.md`** — the operator's global agent instructions, the outermost link
  of the [[design/agents-md-injection|Workspace]] chain injected into the system
  prompt.

## The trust model

**The config home is trusted precisely because it is never the working tree.**
The sandboxed agent's [[design/grant|grant]] admits writes to `cwd` and the
per-session scratch dir alone, so no profile can make `$XDG_CONFIG_HOME`
writable — the protection is *structural*, not a deny-list entry an oversight
could omit ([[map/exarch/policy|policy]]).

- **Structural covers write, and only write.** Most bases *read* `` `xdg-config ``
  and `` `xdg-state `` whole, so tools find their own configuration — which is why
  the files holding our credentials are denied by name, in composition, on the
  same footing as a restrict file
  ([[invariants/a-grant-never-reaches-its-own-key|a-grant-never-reaches-its-own-key]]).
  Everything else here the agent may read, and some of it it is meant to: the
  global `AGENTS.md` and the skill roots are addressed to it.

- **A trusted location still gets a no-authority evaluation.** `config.ral` is
  source, so it is *evaluated*, not parsed (`exarch/src/config.rs`) — under a
  no-authority [[design/grant|grant]] (`Capabilities::deny_all`: `exec`, `net`,
  and `fs` all denied), in a throwaway shell. It can compute a value, cause
  no effect. The asymmetry is deliberate: even a file the agent cannot write is
  evaluated as if it might be hostile, because a redirected endpoint is an
  exfiltration channel and the in-process guard suffices to close it
  ([[design/two-enforcers|two enforcers]]: with `exec` denied there is no route
  to the network).

See also [[design/agents-md-injection|agents-md-injection]],
[[design/exarch-architecture|exarch-architecture]], [[design/grant|grant]],
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]],
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].
