---
generated_at_commit: e2b7067b
generated_at_date: 2026-09-23
covers_paths: [ral/src/]
---

# Map: repl (the ral binary)

`ral/` is the `ral` binary — a thin front-end over [[map/core|ral-core]].
**It is argv dispatch into an engine it boots in process and speaks to only
through the [[design/engine-protocol|engine protocol]], driving one of three
selectable frontends over the REPL session and plugins;** the language,
evaluator, and capability machinery all live in core, and neither front-end
holds a `Shell`.

- *Argv dispatch.* `cli.rs` resolves argv to a `Mode` — interactive
  (carrying a login flag), script, or `-c` (`Command`) — each carrying only
  the flags valid for it; `startup.rs` decides whether this process is the
  shell at all and holds the binary's two engine installers (`repl`,
  `batch`), `main.rs` is the thin dispatch over the answer, and `batch.rs`
  runs the non-interactive modes.
- *One boot, one door.* Batch and the REPL both boot an `IdentityTransport`
  from their installer, then dispatch the `_ral-boot` door
  (`ral/src/boot_door.rs`) as their first run, so both keep `exit N` and the
  `--capabilities` status 2 identically; every evaluation after it — a line,
  a script, a prompt, a hook — is one dispatch through `dispatch_to_report`,
  and every read of engine state is a typed probe.
- *Three frontends.* The interactive REPL presents one `Surface` —
  *minimal* (canonical-stdin fallback), *readline* (the default full editor),
  or *structural*, a ratatui projection of live program state. The structural
  surface is the near-term realisation of the recorded direction
  ([[decisions/260522_repl-architecture|repl-architecture]]); it is selected
  with `--surface` or the rc `surface:` key.

The frontend is a layer *above* the engine. Editor state is host-side, in the
REPL's `ReplHost` and `PluginRuntime`; the `_ed-*` builtins are thin
engine-side doors that ask the host for it through the `` `repl-editor ``
enquiry, and core holds no editor state at all
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).
A guiding constraint: ral's top-level runs carry persistent state, and the
REPL makes that state the thing the loop threads.

## Subsystems

- [[map/repl/startup|startup]] — argv → `Mode`, the engine installers, batch
  execution over its own engine, the build-baked prelude, platform glue
  (`ral/src/main.rs`, `startup.rs`, `cli.rs`, `batch.rs`, `boot_door.rs`,
  `platform.rs`, `build.rs`).
- [[map/repl/loop|loop]] — the `Session` state machine and one-dispatch
  cycle: the two-stage boot, the `ReplHost`, prompt, rc/profile sourcing
  engine-side, value printing, error formatting (`ral/src/repl/session*`,
  `exec.rs`, `host.rs`, `enquiry.rs`, `prompt.rs`, `config*`, `theme.rs`,
  `errfmt.rs`).
- [[map/repl/frontend|frontend]] — the `Frontend` trait and its three
  implementations (minimal, rustyline, structural — the structural surface's
  typed spine, reactive worksheet, handles matrix, vi-mode, and DAG render),
  plus the frontend-neutral fuzzy completion engine, Tab menu, and shared
  highlight table
  (`ral/src/repl/frontend*`, `completion.rs`, `complete.rs`, `worksheet.rs`,
  `cursor.rs`, `highlight_style.rs`).
- [[map/repl/plugins|plugins]] — the plugin runtime, the `_ed-*` editor doors,
  the plugin load doors, and the one ordered keybinding router
  (chord, guard, first match, built-in tail) every frontend dispatches
  through; hooks and aliases live engine-side, editor state host-side, and
  every hook run is a dispatch
  (`ral/src/repl/plugin*`, `plugin/ed_builtins.rs`, `plugin/load.rs`,
  `keybinding.rs`).

## Siblings

[[map/core|core]] is the engine this frontend drives; [[map/exarch|exarch]] is the agent
that embeds the same engine instead of a human at the prompt.

_These pages point at code, they do not restate it. The design rationale lives
in the [[AGENTS|durable layer]]; the formal account is `docs/SPEC.md`._
