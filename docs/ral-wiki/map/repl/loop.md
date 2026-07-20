---
generated_at_commit: 1cff92a
generated_at_date: 2026-07-20
covers_paths: [ral/src/repl.rs, ral/src/repl/session.rs, ral/src/repl/session/, ral/src/repl/exec.rs, ral/src/repl/prompt.rs, ral/src/repl/config.rs, ral/src/repl/theme.rs, ral/src/repl/errfmt.rs, ral/src/repl/cursor.rs, ral/src/repl/worksheet.rs]
---

# Map: repl / loop

`ral/src/repl.rs` is the module-tree root and exposes one verb,
`run_interactive`, which boots a `Session` and drives it to an `ExitCode`
([[map/repl/startup|startup]]). Builtins are shell-scoped: `Session::boot`
hands the REPL's `ral_core::HostSurface` — the
[[map/repl/plugins|`_ed-*` builtins]], `watch`, and the captured job/plugin
commands — to `driver::boot_shell` before any rc file is checked, so the
typechecker and the runtime see one surface by construction.

## The session

**The session is a state machine over one long-lived `Shell`: each
iteration drives a *turn* — read, then a single synchronous core entry —
and the loop is seeded from the live session that the previous turn left
behind.**

`session::Session` (`session.rs`) owns the interactive state: an
`IdentityTransport` wrapping the evaluator `Shell` (the in-process arm of
core's transport seam), the shared `Arc<Mutex<JobTable>>` and
`Arc<Mutex<PluginRuntime>>`, the boxed [[map/repl/frontend|`Frontend`]], a
`pending` buffer queued for re-edit, and the exit code. On a `structural`
build it also owns the `Worksheet` (`worksheet.rs`) — the retained
binding-edge / effect-verdict model the structural surface projects.

- `run` loops `turn` until it returns `Flow::Break` — a frontend `Eof` or
  an `exit` escaping the evaluator.
- `turn` is one cycle: reap children, break if the durable root has been
  cancelled (SIGTERM/SIGHUP or Ctrl-\ — cancellation is one-way, so exit
  with the cause's code rather than deal an unusable prompt),
  `process::clear` any residual interrupt, write the terminal title, render
  the prompt, `read`, and dispatch the `Read` event. `Read::Line` adds to
  history and evaluates; `Read::Edit` becomes next turn's `pending`;
  `Read::Interrupt` clears the signal, cancels the current turn scope, and
  continues; `Read::Eof` breaks. `read` is handed the live `Shell`, the
  prompt, the pending buffer, the `JobTable`, and (structural) the
  worksheet.
- `eval` runs one trimmed line through `exec::step`, recording an
  `exit` code so `run` breaks cleanly.
- Teardown — transport detach, history flush, the
  [[map/repl/jobs|survivor warning]], `JobTable::cleanup`, and
  `ral_core::sandbox::teardown_session` (reverts the session's AppContainer
  grant ACEs and deletes its per-projection profiles on Windows; a no-op
  elsewhere) — lives in
  `Drop for Session`, so it covers a panic unwinding through the owned
  `Session` as well as the orderly exit; a crash mid-turn neither orphans a
  stopped group nor loses history.

`session/boot.rs` does the one-shot setup: `setup_signals` (the whole Unix
disposition table in one place — SIGINT relay, SIGQUIT root-abort,
SIGTERM/SIGHUP term handler, SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE ignore),
`setup_panic_hook` (restore the pre-raw terminal state — termios on Unix,
console mode on Windows — then write a crash log, to a state-dir path
resolved at install time so a changed `HOME` cannot redirect it),
`setup_terminal`
(mark interactive, publish the probed `TERMINAL`), `load_profiles` (login
profiles then rc), `install_default_prompt` (register the default
`Session/"prompt"` hook, `{ return "❯ " }` — after rc sourcing, and only
when no rc `prompt:` key registered one already), and `create_frontend`.
`claim_terminal` runs first in `setup_signals`, while SIGTTIN still has its
default disposition: it parks the shell on SIGTTIN until it is foregrounded
before `tcsetpgrp`, so `ral &` does not seize the terminal from a parent
shell's current job. An rc file is checked through `typecheck` against the
live session and, like a parse error, *reported and skipped* on any type
error — the file has no runnable annotation — while the boot always survives
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]); a
stop signal escaping rc sourcing is reported, not swallowed, so it cannot
orphan a process group. An rc `startup` block registers as the
`Session/"startup"` hook and runs through a **framed hook turn** under
`Denied` terminal authority — a fresh frame whose `let`s do not leak —
never an in-place apply.

## One turn through a framed door

**A turn is one synchronous core entry, and evaluation enters only through
the framed turn doors** ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]]).

`exec.rs::step` is the per-line entry. `execute_input` builds the transport
`Turn` — `Program::Source(trimmed)`, `script_name: "<stdin>"`,
`Capabilities::root()`, no wall, `TurnIo::Inherit`,
`RequestedTerminalAccess::Leased`, `TurnStdin::Inherit` — and dispatches it
through `transport::dispatch_to_report` on the session's
`IdentityTransport`, draining the event stream to the terminal `Report`.
The lifecycle hooks fire around the dispatch from `exec.rs` itself via
`IdentityTransport::with_shell`: [[map/repl/plugins|`pre-exec`]] before,
a pending `chpwd` then `post-exec` after. The door compiles and typechecks
against the live session (`shell.session_schemes()`, the one name→scheme
seed — [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]),
then evaluates the annotated comp under the installed turn frame, since the
inference pass always runs
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).

It matches the one flat `Report`:

- `Static` — a parse, type, or host failure that never reached evaluation,
  printed from its `Diagnostics` arm. The turn is not run.
- `Ran` — a compiled turn. `Ok` prints via `print_result` (and, on a
  `structural` build, records the bind into the worksheet);
  `Break::Exit` ends the loop (clamped through `platform::exit_byte`);
  `Break::Error` renders a runtime diagnostic; `Break::Stopped` (Unix)
  parks the pipeline in the [[map/repl/jobs|job table]].

The mobile-state install on every outcome is core's contract — a top-level
turn is a resume point ([[map/core/evaluator|evaluator]]). The same door
backs every host program the REPL holds as a value: the prompt body, an rc
startup block, and a plugin hook are *registered hooks* run as
`Program::Hook` turns ([[map/repl/plugins|plugins]]), under
`RequestedTerminalAccess::Denied` unless the site leases the terminal, so a
hook that is not a command can never foreground a child.

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

- `prompt.rs` — runs the registered `Session/"prompt"` hook through a
  capture-into-string turn (`Program::Hook`, `TurnIo::Capture`, `Denied`
  terminal): its return value is the prompt, a returned unit falls back to
  its captured stdout. USER, CWD, and STATUS are ambient pseudo-variables
  the prompt body reads directly. Plugin `prompt` hooks may transform the
  result; the terminal title is written separately. A failing prompt body
  falls back to the default `❯ ` beside its per-render diagnostic, and the
  session survives so the user can rebind it.
- `config.rs` — rc is ral source returning a map; recognised keys (`env`,
  `prompt` — registered as the `Session/"prompt"` hook — `bindings`,
  `aliases`, `edit_mode`, `bell`, `surface`, `recursion_limit`, `plugins`,
  `startup`, `theme`) map to REPL state, unknown keys ignored.
- `theme.rs` — `OutputTheme` (the `value_prefix`, default `"=> "`, and an
  optional `value_color`, default yellow) governs value rendering;
  process-global behind an `RwLock`, set once from rc.
- `errfmt.rs` — plugin-diagnostic formatting (the rendered/deferred plugin
  fault lines), alongside core's full ariadne renderer.
- `cursor.rs` — Unix cursor-column query for the zsh-style partial-line
  marker.
