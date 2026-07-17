---
status: proposed
---

# REPL architecture direction

A design study of where the REPL should go. Three architectures were weighed:

- a block notebook with persistent-turn semantics and structured cards;
- a stream console extending the current line-editing substrate;
- a hybrid workbench combining a shell transcript with dockable panes.

The recorded recommendation is sequenced:

- **stream console near-term**, since the current codebase already does the
  line-editing part well enough to serve as the substrate;
- **hybrid workbench long-term**, built on full-screen / pane machinery
  (Ratatui + Crossterm).

A guiding observation: ral's top-level turns are semantically special — they
carry persistent state — and the REPL should make that visible rather than hide
it.

This is a direction, not yet a committed implementation. The stream-console
substrate is in place — `rustyline` line-editing over a scrollback transcript —
and the REPL's editor builtins already live in the right layer
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]). The
Ratatui + Crossterm stack the long-term workbench would build on is now exercised
in-repo by [[design/exarch-architecture|exarch]]'s full-screen TUI, though
`ral`'s REPL has not adopted it.

See also [[map/core|core]].
