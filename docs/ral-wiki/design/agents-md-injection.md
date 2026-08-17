# AGENTS.md injection: project guidance that cannot widen the grant

**exarch injects `AGENTS.md` instruction files into the system prompt as a
*Workspace* section — project guidance that steers behaviour but cannot widen the
[[design/grant|grant]].** The files add prompt *text*, never capabilities, so the
question of whether a given `AGENTS.md` is trusted is decoupled from the question
of what it can cause: it can cause nothing.

## Discovery

`discover_agents(cwd, config_dir)` (`exarch/src/prompt.rs`) gathers the files
*outermost first*, so the most specific file is read last and its recency
dominates:

- **The operator's `<config>/AGENTS.md` leads** — from the trusted
  [[design/exarch-config-dir|config dir]], the same root `config.ral` loads from.
- **Then every repo `AGENTS.md` from the git root down to `cwd`**, deepest last.
  The ancestor walk stops at the first directory holding a `.git` *entry* — file
  or directory, so worktrees and submodules count — which bounds discovery to the
  project the agent was launched in.
- **Outside a git repo, only `cwd/AGENTS.md`.** The bare ancestor chain is *not*
  followed up into unrelated parents (the chain is truncated to one when no `.git`
  root is found).

## Placement and semantics

- **One `Workspace` section, present whenever any `AGENTS.md` is found.** It is
  ordered after the `Host` section and before optional `Skills`/`Surfacing`
  sections, and is loaded *regardless of* `--system` — orthogonal to the
  persona and to interactive surfacing.
- **Existence is the only gate**, checked through `ral_core::path::exists`; the
  reads reuse `read_files`'s `[io-door:silent:system-prompt-files]` door — no new
  I/O door is opened for `AGENTS.md`.

## The authority distinction

**These files add prompt text, never authority.** That makes trust orthogonal to
effect, and the contrast with `config.ral` is exact:

- **A cwd `AGENTS.md` is *untrusted*.** It lives in the agent's own writable tree,
  so unlike `config.ral` exarch cannot assume the operator authored it.
- **It does not need to be trusted, because it cannot touch the
  [[design/grant|grant]].** The accepted tradeoff is sound: a repository you enter
  can steer the agent's *instructions*, never its *authority*.
- **`config.ral` is the mirror image** — it *is* trusted (it lives in the
  [[design/exarch-config-dir|config dir]] the agent cannot reach) and it *can*
  cause effects (it redirects transport), which is exactly why it earns a
  no-authority evaluation ([[decisions/260613_provider-config-ral-script|provider-config-ral-script]]).
  A workspace file needs no such defence: text is inert.

The set of builtins is still discovered at runtime via `help`, which reads the
live resolver and cannot drift — `AGENTS.md` adds project context, not a tool
reference.

See also [[design/exarch-config-dir|exarch-config-dir]],
[[design/exarch-architecture|exarch-architecture]], [[design/grant|grant]],
[[decisions/260613_provider-config-ral-script|provider-config-ral-script]],
[[decisions/260601_xdg-resolver-consolidation|xdg-resolver-consolidation]].
