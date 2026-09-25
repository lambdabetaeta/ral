---
status: active
---

# `within [dir:]` is the working directory's local-state handler

**The working directory is one state cell. `cd` is its *set*; `$CWD`, `cwd`
and every relative path are its *get*. `within [dir: d] M` is the local-state
handler for the cell: it saves the cell, sets it to `d`, runs `M`, and restores
the saved cell on every exit.** Blocks, functions and branches are not
handlers, so a `cd` in one passes through to the nearest handler; a pipeline
stage and a `spawn` get a copy of the cell; the session is the outermost
handler. The equations of state — a get after a set sees the set, setting what
was got changes nothing — now hold inside `within` as everywhere else, exactly: the cell is one path,
and ral exports no `OLDPWD`.

## What was wrong

The store kept two slots: `Context::dir`, the override `within [dir:]`
installed, and `Context::cwd`, the `Cwd` that `cd` wrote; "here" was
`dir.or(cwd.current)`. `within` saved and restored `dir` alone, so it handled
*get* and forwarded *set*. A `cd` in its body resolved against the override,
landed in the slot the override shadowed, and surfaced only when the scope
exited:

- `cd /tmp; within [dir: /usr] { cd /usr/bin; pwd }; pwd` printed `/usr`, then
  `/usr/bin`: the set invisible to the get inside, and leaking out;
- `cd /tmp; within [dir: /usr] { cd $CWD }; pwd` printed `/usr`: setting the
  cell to its own value moved the session.

SPEC §9.2 documented the first as "`dir` is an override, not a `cd`".

## What was decided

- `Context::dir` is deleted. `WithinUndo` holds the saved `Cwd` and puts it
  back on return, failure, `exit`, cancellation and panic: the paths every
  `Frame::Within` already took, so the machine did not change.
- Entry sets the cell. It is not a `cd`: no `shell.chdir` check, no
  observation. Its validation is unchanged.
- `Cwd`'s `previous`, kept only to export `OLDPWD`, is deleted. It made
  `cd $CWD` and `cd a; cd b` observably differ from a no-op and from `cd b`,
  and ral has no `cd -` to want it. Children get no `OLDPWD`, an inherited one
  is removed, and `within [env:]` still refuses to set it.
- A `within` without `dir` handles no cell; a `cd` inside `within [env: …]`
  persists, as in any block.
- `Shell::with_cwd`, which only tests called, is deleted; `seed_cwd` states a
  directory. The wire mirror carries the one cell.
- `chpwd` observes the line boundary: the REPL compares the session's
  directory before and after each line and fires iff they differ, so a `cd`
  a `within` undid fires nothing. `cd` records nothing, and the `last-chpwd`
  reading is gone (`PROTOCOL_VERSION` 10).

## Why the environment stays an overlay

`within [env: …]` saves and restores too, and needs nothing more: ral has no
`setenv`, so inside a body the environment is read-only, and an overlay
restored on exit is already the whole handler for a cell nobody can set. The
directory has `cd`, so its scope must handle the set as well, or the scope
leaks.

## In the code

`Cwd` and `Shell::{cwd, apply_chdir, enter_cwd, restore_cwd}` in
`core/src/types/shell/cwd.rs`; `Context::cwd` in `context.rs`;
`WithinScope::enter` and `WithinUndo::apply` in `core/src/evaluator/scope.rs`;
the probes as tests in `core/tests/top_level_vs_block.rs` §(6c); the
`chpwd` boundary in `ral/src/repl/exec.rs::step`. Refines
[[decisions/260826_the-evaluator-steps-closures|the-evaluator-steps-closures]]
S1/S10: `within [dir:]` stays the scoped form, and is now a handler.
[[decisions/260731_one-walk-one-anchor|one-walk-one-anchor]]'s anchor is the
cell, with no precedence left to re-derive.

Plan: `dev/docs/plans/260925_within-dir-is-local-state.md`.
