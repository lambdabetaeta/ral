---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
covers_paths: [ral/src/repl.rs, ral/src/repl/session.rs, ral/src/repl/session/, ral/src/repl/exec.rs, ral/src/repl/prompt.rs, ral/src/repl/config.rs, ral/src/repl/theme.rs, ral/src/repl/errfmt.rs, ral/src/repl/cursor.rs]
---

# Map: repl / loop

`ral/src/repl.rs` is the module-tree root and exposes one verb,
`run_interactive`, which boots a `Session` and drives it to an `ExitCode`.

## The session

**The session is a state machine over one long-lived `Shell`: each
iteration drives a *turn* — read, then a single synchronous core entry —
and the loop is seeded from the live session that the previous turn left
behind.**

`session::Session` (`session.rs`) owns the interactive state: the evaluator
`Shell`, the shared `Arc<Mutex<JobTable>>` and `Arc<Mutex<PluginRuntime>>`,
the boxed [[map/repl/frontend|`Frontend`]], a `pending` buffer queued for
re-edit, and the exit code. On a `structural` build it also owns the
`Worksheet` — the retained binding-edge / effect-verdict model the
structural surface projects ([[map/repl/frontend|frontend]] owns its
internals).

- `run` loops `turn` until it returns `Flow::Break` — a frontend `Eof` or
  an `exit` escaping the evaluator.
- `turn` is one cycle: reap children, `process::clear` any residual
  interrupt, write the terminal title, render the prompt, `read`, and
  dispatch the `Read` event. `Read::Line` adds to history and evaluates;
  `Read::Edit` becomes next turn's `pending`; `Read::Interrupt` clears the
  signal and continues; `Read::Eof` breaks. `read` is handed the live
  `Shell`, the prompt, the pending buffer, the `JobTable`, and (structural)
  the worksheet.
- `eval` runs one trimmed line through `exec::step`, recording an
  `exit` code so `run` breaks cleanly.
- Teardown — history flush and `JobTable::cleanup` — lives in `Drop for
  Session`, so it covers a panic unwinding through the owned `Session` as
  well as the orderly exit; a crash mid-turn neither orphans a stopped
  group nor loses history.

`session/boot.rs` does the one-shot setup, each function called once:
`setup_signals` (Unix dispositions — SIGINT relay, SIGQUIT root-abort,
SIGTERM/SIGHUP term handler, SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE ignore),
`setup_panic_hook` (restore termios, write a crash log), `setup_terminal`
(mark interactive, publish the probed `TERMINAL`), `install_default_prompt`
(bind the default `RAL_PROMPT` thunk, `{ return "❯ " }`, before rc sourcing
so the rc `prompt:` key overwrites it), `load_profiles` (login profiles
then rc), and `create_frontend`. `claim_terminal` runs first in
`setup_signals`, while SIGTTIN still has its default disposition: it parks
the shell on SIGTTIN until it is foregrounded before `tcsetpgrp`, so `ral &`
does not seize the terminal from a parent shell's current job. An rc file is
checked through `typecheck` against the live session and, like a parse
error, *reported and skipped* on any type error — the file has no runnable
annotation — while the boot always survives
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]); a
stop signal escaping rc sourcing is reported, not swallowed, so it cannot
orphan a process group. An rc `startup` block runs through the **value turn
door** under `Denied` terminal authority — a fresh frame whose `let`s do not
leak — never an in-place apply. For the readline surface, the frontend's
`ExternalPrinter` is wired onto the shell via `shell.set_stdout` so `watch`
background output lands above the live prompt.

## One turn through a framed door

**A turn is one synchronous core entry, and evaluation enters only through
the framed turn doors** ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

`exec.rs::step` is the per-line entry. `execute_input` builds a
`TurnRequest` — `script_name: "<stdin>"`, `Capabilities::root()`, no wall,
`TurnIo::Inherit`, `RequestedTerminalAccess::Leased`, `TurnStdin::Inherit`,
no surface sink, and a `ReplLifecycle` carrying the
[[map/repl/plugins|`pre-exec` / `chpwd` / `post-exec` hooks]] — and calls
`shell.run_source_turn(trimmed, req)`. The door compiles and typechecks
against the live session (`shell.session_schemes()`, the one name→scheme
seed — [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]),
then evaluates the annotated comp under the installed turn frame, since the
inference pass always runs
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).

It matches the one flat `TurnReport`:

- `Static` — a parse or type failure that never reached evaluation. A parse
  error renders via the compact REPL path (`errfmt.rs`) or the full ariadne
  renderer; a type error renders ariadne. The turn is not run.
- `Ran` — a compiled turn. `Ok` prints via `print_result` (and, on a
  `structural` build, records the bind into the worksheet);
  `Escape::Exit` ends the loop; `Break::Error` renders a runtime
  diagnostic; `Escape::Stopped` (Unix) parks the pipeline in the
  [[map/repl/jobs|job table]].

The mobile-state install on every outcome is core's contract — a top-level
turn is a resume point ([[map/core/evaluator|evaluator]]). The same door
backs every host: a prompt thunk, an rc startup block, and a plugin hook
enter through `run_value_turn` (a thunk applied to args) under
`RequestedTerminalAccess::Denied`, so a hook that is not a command can never
foreground a child.

## The selectable frontend

The loop drives a boxed `Frontend`, chosen at boot from a `Surface`:

- `Surface::Minimal` — the canonical-stdin editor.
- `Surface::Readline` (the default) — the rustyline editor.
- `Surface::Structural` — the ratatui projection surface, behind the
  default-on `structural` feature.

`create_frontend` resolves it: the capability gate forces the minimal editor
on a dumb terminal whatever was asked, otherwise the surface preference
decides; a `Structural` request that cannot be honoured (no raw mode, or a
build without the feature) warns and falls back to readline rather than
degrading silently. The preference is set by the `--surface` flag (CLI wins)
or the rc `surface:` key. The three implementations, the `Frontend` trait,
the structural worksheet projection, and completion live in
[[map/repl/frontend|frontend]].

## Prompt, rc, theme

- `prompt.rs` — computes per-prompt bindings (USER, CWD, STATUS) onto the
  live shell and renders the `RAL_PROMPT` value: a block runs through the
  value turn door under `Denied` (its return value, or its captured stdout
  when it returns unit), anything else is its display form; bound at boot,
  so always present. A `prompt` hook may transform the result; the terminal
  title is written separately. A failing prompt thunk falls back to the
  default `❯ ` beside its per-render diagnostic, and the session survives so
  the user can rebind it.
- `config.rs` — rc is ral source returning a map; recognised keys (`env`,
  `prompt`, `bindings`, `aliases`, `edit_mode`, `bell`, `surface`,
  `recursion_limit`, `plugins`, `startup`, `theme`) map to REPL state,
  unknown keys ignored.
- `theme.rs` — `OutputTheme` (the `value_prefix`, default `"=> "`, and an
  optional `value_color`, default yellow) governs value rendering;
  process-global behind an `RwLock`, set once from rc.
- `errfmt.rs` — the compact REPL parse-error path and plugin-diagnostic
  formatting, alongside core's full ariadne renderer.
- `cursor.rs` — Unix cursor-column query for the zsh-style partial-line
  marker.
