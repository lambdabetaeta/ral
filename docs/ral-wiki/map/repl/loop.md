---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [ral/src/repl.rs, ral/src/repl/session.rs, ral/src/repl/session/, ral/src/repl/exec.rs, ral/src/repl/prompt.rs, ral/src/repl/config.rs, ral/src/repl/theme.rs, ral/src/repl/errfmt.rs, ral/src/repl/cursor.rs]
---

# Map: repl / loop

`ral/src/repl.rs` is the module-tree root and exposes one verb,
`run_interactive`, which boots a `Session` and drives it to an `ExitCode`.

## The session

`session::Session` (`session.rs`) owns the long-lived interactive state: the
evaluator `Shell`, the shared `Arc<Mutex<JobTable>>` and
`Arc<Mutex<PluginRuntime>>`, the boxed [[map/repl/frontend|`Frontend`]], a
`pending` buffer queued for re-edit, and the exit code. `run` loops `turn`
until the frontend reports `Eof` or the evaluator escapes with `exit`.
Teardown — history flush and `JobTable::cleanup` — lives in `Drop for Session`,
so it covers a panic unwinding through the owned `Session` as well as the
orderly exit; a crash mid-turn neither orphans a stopped group nor loses
history.

`turn` is one cycle: reap children, clear any residual interrupt, render the
prompt, `read`, and dispatch the resulting `Read` event. A `Read::Line` is
added to history and evaluated; a `Read::Edit` becomes next turn's `pending`.

`session/boot.rs` does the one-shot setup, each function called once:
`setup_signals` (Unix dispositions — SIGINT relay, SIGTSTP/SIGTTOU/SIGPIPE
ignore), `setup_panic_hook` (restore termios, write a crash log), `setup_terminal`
(probe and bind `TERMINAL`), `install_default_prompt` (bind the default
`RAL_PROMPT` thunk, `{ return "❯ " }`, before rc sourcing so the rc `prompt:`
key overwrites it), `load_profiles` (login profiles then rc), and
`create_frontend`. `claim_terminal` runs first in `setup_signals`, while SIGTTIN
still has its default disposition: it parks the shell on SIGTTIN until it is
foregrounded before `tcsetpgrp`, so `ral &` does not seize the terminal from a
parent shell's current job. An rc file is checked through `typecheck` and, like
a parse error, *reported and skipped* on any type error — the file has no
runnable annotation — while the boot always survives
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]); a stop
signal escaping rc sourcing is reported, not swallowed, so it cannot orphan a
process group. The
frontend's
`ExternalPrinter` is wired onto `shell.local.io.stdout` so `watch` background
output lands above the live prompt.

## One input

`exec.rs::step` is the per-line entry. `execute_input` runs `compile_and_typecheck`
over the trimmed input seeded from the live session (`shell.session_schemes()`, the
one name→scheme seed —
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]]), since the
inference pass always runs
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]). A `Compiled`
outcome evaluates the *annotated* comp with `eval_top_level`; any type error is
fatal — the turn is reported and not run. `pre-exec` / `chpwd` / `post-exec`
[[map/repl/plugins|lifecycle hooks]] fold around the call. It
matches the `Settled<Value>` outcome: `Ok` prints via `print_result`,
`Escape::Exit` ends the loop, `Error` renders an ariadne diagnostic, and
`Escape::Stopped` (Unix) parks the pipeline in the [[map/repl/jobs|job table]].
The mobile-state install on every outcome is core's contract — a top-level turn
is a resume point ([[map/core/evaluator|evaluator]]).

## Prompt, rc, theme

- `prompt.rs` — computes per-prompt bindings (USER, CWD, STATUS) onto the live
  shell and renders the `RAL_PROMPT` value (a block is evaluated, anything
  else is its display form; bound at boot, so always present), lets a
  `prompt` hook transform the result, writes the terminal title.  A failing
  prompt thunk falls back to the default `❯ ` beside its per-render
  diagnostic, and the session survives so the user can rebind it.
- `config.rs` — rc is ral source returning a map; recognised keys (`env`,
  `prompt`, `bindings`, `aliases`, `edit_mode`, `bell`, `recursion_limit`,
  `plugins`, `startup`, `theme`) map to REPL state, unknown keys ignored.
- `theme.rs` — `OutputTheme` (the `value_prefix` and optional `value_color`)
  governs value rendering; process-global behind an `RwLock`, set once from rc.
- `errfmt.rs` — the compact REPL parse-error path and plugin-diagnostic
  formatting, alongside core's full ariadne renderer.
- `cursor.rs` — Unix cursor-column query for the zsh-style partial-line marker.
