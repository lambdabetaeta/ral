---
generated_at_commit: 2df6db85
generated_at_date: 2026-06-10
covers_paths: [ral/src/]
---

# Map: repl (the ral binary)

`ral/` is the `ral` binary — a thin interactive frontend over
[[map/core|ral-core]]. It owns argv dispatch, the read-eval-print loop, the
line-editing frontends, the plugin and editor surface, and Unix job control;
the language, evaluator, and capability machinery all live in core.

The frontend is a layer *above* the engine. Its line-editor builtins (`_ed-*`)
and editor-state types live here, not in core
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]); core stores the editor
context type-erased as `Box<dyn Any>` in `ReplScratch` and never inspects it.
The design direction — stream console near-term, hybrid workbench long-term —
is recorded in [[decisions/260522_repl-architecture|repl-architecture]]. A guiding constraint:
ral's top-level turns carry persistent state, and the REPL makes that state the
thing the loop threads.

## Subsystems

- [[map/repl/startup|startup]] — `main.rs`: argv → `Mode`, batch execution, the
  build-baked prelude, platform glue (`ral/src/main.rs`, `platform.rs`,
  `build.rs`).
- [[map/repl/loop|loop]] — the `Session` state machine and one-turn cycle: boot,
  prompt, rc/profile sourcing, value printing (`ral/src/repl/session*`,
  `exec.rs`, `prompt.rs`, `config.rs`, `theme.rs`).
- [[map/repl/frontend|frontend]] — the `Frontend` trait and its two implementations,
  plus tab completion (`ral/src/repl/frontend*`, `complete.rs`).
- [[map/repl/plugins|plugins]] — the plugin runtime, the `_ed-*` editor builtins, and
  the captured job/plugin commands (`ral/src/repl/plugin*`,
  `plugin_ed_builtins.rs`, `host_handlers.rs`, `keybinding.rs`).
- [[map/repl/jobs|jobs]] — process-group job control (`ral/src/jobs.rs`).

## Siblings

[[map/core|core]] is the engine this frontend drives; [[map/exarch|exarch]] is the agent
that embeds the same engine instead of a human at the prompt.

_These pages point at code, they do not restate it. The design rationale lives
in the [[AGENTS|durable layer]]; the formal account is `docs/SPEC.md`._
