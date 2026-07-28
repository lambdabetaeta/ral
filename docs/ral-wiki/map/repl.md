---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
covers_paths: [ral/src/]
---

# Map: repl (the ral binary)

`ral/` is the `ral` binary — a thin interactive frontend over
[[map/core|ral-core]]. **It is argv dispatch into a run through core's framed
door, driving one of three selectable frontends over the REPL session,
plugins, and Unix jobs;** the language, evaluator, and capability machinery all
live in core.

- *Argv dispatch.* `cli.rs` resolves argv to a `Mode` — interactive,
  login, script, or `-c` (`Command`) — each carrying only the flags valid for
  it; `main.rs` is the thin dispatch over it, and `batch.rs` runs the
  non-interactive modes.
- *Framed run.* Every evaluation, batch or interactive, enters core through
  the same *framed run door* (`shell.run`): a run is one
  synchronous call carrying its own policy — capabilities, limits, IO regime,
  terminal access, lifecycle hooks ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]).
- *Three frontends.* The interactive REPL presents one `Surface` —
  *minimal* (canonical-stdin fallback), *readline* (the default full editor),
  or *structural*, a ratatui projection of live program state. The structural
  surface is the near-term realisation of the recorded direction
  ([[decisions/260522_repl-architecture|repl-architecture]]); it is selected
  with `--surface` or the rc `surface:` key.

The frontend is a layer *above* the engine. Its line-editor builtins (`_ed-*`)
and editor-state types live here, not in core
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]); core stores the editor
context type-erased as `Box<dyn Any>` in `ReplScratch` and never inspects it.
A guiding constraint: ral's top-level runs carry persistent state, and the
REPL makes that state the thing the loop threads.

## Subsystems

- [[map/repl/startup|startup]] — argv → `Mode`, batch execution
  through the framed door, the build-baked prelude, platform glue
  (`ral/src/main.rs`, `cli.rs`, `batch.rs`, `platform.rs`, `build.rs`).
- [[map/repl/loop|loop]] — the `Session` state machine and one-run cycle: boot,
  prompt, rc/profile sourcing, value printing, error formatting
  (`ral/src/repl/session*`, `exec.rs`, `prompt.rs`, `config.rs`, `theme.rs`,
  `errfmt.rs`).
- [[map/repl/frontend|frontend]] — the `Frontend` trait and its three
  implementations (minimal, rustyline, structural — the structural surface's
  typed spine, reactive worksheet, handles matrix, vi-mode, and DAG render),
  plus the frontend-neutral fuzzy completion engine, Tab menu, and shared
  highlight table
  (`ral/src/repl/frontend*`, `completion.rs`, `complete.rs`, `worksheet.rs`,
  `cursor.rs`, `highlight_style.rs`).
- [[map/repl/plugins|plugins]] — the plugin runtime, the `_ed-*` editor builtins,
  the captured job/plugin commands, and the one ordered keybinding router
  (chord, guard, first match, built-in tail) every frontend dispatches
  through; plugin hooks run inside a framed run
  (`ral/src/repl/plugin*`, `plugin_ed_builtins.rs`, `host_handlers.rs`,
  `keybinding.rs`).
- [[map/repl/jobs|jobs]] — process-group job control (`ral/src/jobs.rs`).

## Siblings

[[map/core|core]] is the engine this frontend drives; [[map/exarch|exarch]] is the agent
that embeds the same engine instead of a human at the prompt.

_These pages point at code, they do not restate it. The design rationale lives
in the [[AGENTS|durable layer]]; the formal account is `docs/SPEC.md`._
