---
generated_at_commit: efbf42011e7c77f2b8c4f6ba1df0563ef202d650
generated_at_date: 2026-10-27
covers_paths: [ral/src/repl.rs, ral/src/repl/session.rs, ral/src/repl/session/, ral/src/repl/exec.rs, ral/src/repl/host.rs, ral/src/repl/enquiry.rs, ral/src/repl/prompt.rs, ral/src/repl/config.rs, ral/src/repl/config/, ral/src/repl/theme.rs, ral/src/repl/errfmt.rs, ral/src/repl/cursor.rs, ral/src/boot_builtin.rs, ral/src/surface.rs]
---

# Map: repl / loop

`ral/src/repl.rs` is the module-tree root and exposes one verb,
`run_interactive`, which boots a `Session` and drives it to an `ExitCode`
([[map/repl/startup|startup]]).

## The session

**The REPL is a front-end that holds no `Shell`: it boots an
`IdentityTransport` from the `repl` installer and speaks only the
[[map/core/engine-protocol|engine protocol]] — every evaluation a dispatch,
every read of engine state a probe — so what it can do is exactly what the
protocol carries** ([[design/engine-protocol|engine-protocol]]).

`session::Session` (`session.rs`) owns the transport, the REPL's `Host`
(`ReplHost`), the boxed [[map/repl/frontend|`Frontend`]], a `pending` buffer
queued for re-edit, the probed terminal, the exit code, and the
`TerminalClaim` — last, so it drops last (below). There is
no job table: ral does not suspend
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

- `Session::boot` is two-staged. Stage one is `IdentityTransport::boot` with
  `startup::engine::INSTALLERS` and an `Attach` that configures nothing —
  login environment adoption has already run in `main::run`; the `repl` recipe
  does what precedes rc (below). A
  refusal prints its sentence and exits 2. Stage two is the host's first
  dispatch: `Program::Hook` on the `Session "boot"` hook, the `_ral-boot`
  builtin, applied to `Boot {no-rc, recursion-limit, capabilities}`
  (`ral/src/boot_builtin.rs`). `capabilities` holds absolute paths:
  `Boot::profiles` anchors each argument at the invocation cwd before the
  builtin runs, since the rc and earlier profiles may `cd` first, and the builtin
  refuses a relative one before sourcing anything. `boot_builtin::settle` reads its `Ending`: `Settled`
  carries the rc's `RcSettings`, a `--capabilities` failure is `Raised`
  status 2, an rc's `exit N` is `Exited(N)`, and either ends the session
  there. The host then applies `--surface` over the rc's choice, keeps the
  output theme, and dispatches `Session "startup"` if the rc registered one.
- `run` loops `iterate` until it returns `Flow::Break` — a frontend `End`, an
  `exit`, or a severed transport.
- `iterate` is one cycle: break if the `session-ended` reading names a cause
  (SIGTERM/SIGHUP or Ctrl-\ cancelled the durable root — cancellation is
  one-way, so exit with the cause's code rather than deal an unusable
  prompt), `process::clear` any residual interrupt, write the terminal title
  from the `cwd` reading, render the prompt, `read`, and act on the `Read`.
  `Read::Line` adds to history and evaluates; `Read::Edit` becomes next
  iteration's `pending`; `Read::Interrupt` just continues, nothing being in
  flight at the prompt; `Read::End(code)` records the exit status and breaks. After `read` returns
  the session flushes deferred plugin diagnostics, the one boundary, and
  checks `session-ended` again before interpreting input. `read` is handed
  the transport, the prompt, the pending buffer, and on Unix the exclusive
  `Input` reader.
- The Unix ambient forwarder first calls `ControlSender::hear`, cancelling
  the engine's scopes synchronously, then ends `Input` on a root cause.
  Its persistent shutdown flag and repeated thread-directed interrupt cover
  the interval between the root check and the blocking read; the
  [[map/repl/frontend|frontend]] returns through normal
  cleanup. The signal thread never exits the process or restores its terminal.
- `eval` runs one trimmed line through `exec::step`, recording an `exit` code
  so `run` breaks cleanly.
- Teardown — the `workers` reading, history flush, a
  one-time notice naming any worker still `running` or `stopping`
  (`host::teardown_notice`, over `WorkerRow`s), and
  `ral_core::authority::sandbox::teardown_session` (deletes the session's
  per-projection AppContainer profiles on Windows; a no-op elsewhere) — lives
  in `Drop for Session`, so it covers a panic unwinding through the owned
  `Session` as well as the orderly exit.

`session/boot.rs` is the process-level setup that precedes the engine and
the frontend the rc settles on after it: `setup_signals` (the whole Unix
disposition table in one place — SIGINT `interrupt_handler`, which raises the
foreground interrupt and nothing else, SIGQUIT root-abort, SIGTERM/SIGHUP term
handler, SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE ignore — every entry set through
core's `process::ignore` / `process::install`, the one place that snapshots the
inherited `SIG_IGN`s before the first change so `reset_child_signals` can hand
them on: the nohup rule), `claim_terminal` (run after
SIGTTOU is ignored, since its `tcsetpgrp` runs from a group not yet in the
foreground, and before SIGTTIN is: it parks the shell on SIGTTIN until it is
foregrounded (at most 64 parks, then an error asking whether ral was started in the background of a shell that has since exited; SIGTTIN is set to `SIG_DFL` through core's `process::default` first), so `ral &` does not seize the terminal from a parent shell's
current job; it answers a `TerminalClaim` naming the group it left, if it
left one, which hands the terminal back on drop — else a parent doing no job
control, `vim`'s `:sh` or `su`, stops on its next terminal read),
`setup_panic_hook` (restore the
pre-raw terminal state — termios on Unix, console mode on Windows — then write
a crash log, a new file `crash-<ts>-<random>.log` in ral's state directory
(`$XDG_STATE_HOME/ral`), held as an `OwnDir` from install time so a changed
`HOME` cannot redirect it; with no state base, the panic message goes to
stderr), and `create_frontend`, which hands the line editor its history file,
`$XDG_STATE_HOME/ral/history` (`config::dirs_history`; none without a state
base, and then no history).

## The boot, engine-side

The `repl` installer's recipe (`startup::engine::boot_repl`) boots the REPL
surface — the batch surface plus the [[map/repl/plugins|`_ed-*` builtins and
the plugin load builtins]] — through `boot_shell`, so the typechecker and the
runtime see one surface by construction; then exit hints, the
interactive flag, `$TERMINAL`, and the boot builtin's registration. The umask
is inherited from `login(1)` or PAM unless the rc sets `umask:`.

The boot builtin (`_ral-boot`, one-shot: it unregisters its own hook) does the
rest in a run, since startup files evaluate ral: `source_rc_file`
(`config/source.rs` — the rc only; `Boot` has no `login` field), the CLI's
`--recursion-limit` after the rc, `--capabilities` through
`shell::load::profile::apply_session_profiles` (the user's ceiling is the last word,
and a session frame it pushes survives the builtin's run), and
`install_default_prompt` (register the default `Session/"prompt"` hook,
`{ return "❯ " }`, only when no rc `prompt:` key registered one). It answers
the rc's `RcSettings {edit-mode, bell, surface, theme, startup}` — the only
part of the rc the host needs; everything else the rc configures lands on the
engine's shell.

The rc returns a configuration record, or `()` to leave the defaults. An
`exit N` in the rc ends the session with status N; any other failure is
reported and the boot goes on. The rc goes through `shell::load::evaluate_source`,
compiled against the live session and the `FileId` its text is registered
with, so an alias or function it defines keeps naming the rc for the whole
session. It compiles under a **return contract**
(`ral_core::typecheck::ReturnContract`): `contract::declared(Form::Rc)`, whose
closed keyset the rc's own returned *row* is held to — the same rule
`within`/`grant` options get, extended to a program's own return value. It is
the inferred row and not the syntax that produced it, so a key misspelled
inside a spread is caught with one written out; a return carrying no row (a
`Map`) meets the same keyset in `RcPlan::decode` instead. A file that does
not compile — a parse or type error, a broken contract among the latter — is
*reported and skipped*: `errfmt::report_failed_load` prints the caret report
its `Error::uncompiled` carries ([[map/core/diagnostics|diagnostics]]), then
names the file `skipped, since it does not compile`, as against `sourcing
stopped at the error above` for one that failed partway; the boot always survives
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]), the
whole rc unapplied rather than only the offending key. A *computed* rc return
(a bound variable, a call) is invisible to the contract and stays on
`RcPlan::decode`, which meets the same keyset and field types, reading the
shell only, and refuses the whole rc on an unknown key, a malformed value, an
unknown theme key, a prompt or startup block `vet_hook` rejects, or an alias
`HandlerEntry::vet` rejects (a non-function under `aliases:` is pointed at
`bindings:`); only then does `RcPlan::apply` commit, with no refusal left
(a plugin that fails to load is reported and skipped, an `exit` propagates). An rc `startup` block registers as
the `Session/"startup"` hook, and the host dispatches it as a hook run under
`Denied` terminal authority — a fresh frame whose `let`s do not leak.

The rc's `env:` sets environment entries only; lexical values belong under
`bindings:`. `umask:` takes one to four octal digits as a string, at most
`0777`, and sets the process mask before plugins load. `limits:` validates
`open-files`, `core-bytes`, and `stack-bytes` against ral's hard limits, then
stores child soft limits without changing ral's own limits. Grant limits
apply after these session limits and win. Both keys are refused on Windows.

## Every evaluation is a dispatch

**Each line, prompt, and hook is one `Run` through `dispatch_to_report` with
the REPL host, and the engine's run entry compiles, typechecks, and runs it against
the live session** ([[internals/a-turn-end-to-end|a run, end to end]]).

`exec.rs::step` is the per-line entry. `line_run` builds the `Run` —
`Program::Source(line)`, `origin: SourceOrigin::named("<stdin>")`, `GrantStack::root()`,
no wall, `RunIo::Inherit`, `RequestedTerminalAccess::Leased`,
`RunStdin::Inherit` — and `ReplHost::dispatch` sends it. Around it the
lifecycle hooks run as dispatches of their own. Before, `pre_exec` folds the
plugins' `pre-exec` hooks over the typed line in load order, each dispatched
with `{src}`: a `Str` result is the next hook's line and, last, the line
`line_run` carries; `()`, an `exit`, a fault or a wrong-shaped value
([[map/repl/plugins|plugins]]) leaves it. After, `fire` runs `chpwd`
with `{old, new}` if the `cwd` readings, taken before and after the line, are
both present and differ (a working directory that cannot travel as text reads
as none) — the session is the working directory's outermost handler, so a `cd`
a `within [dir:]` undid fires nothing — then `post-exec` with
`{src, status}`, `src` being the line that ran — for every line, a static
failure's status included. An `Err(Severed)` prints the cause and ends the
session.

It matches the one flat `Report`:

- `Static` — a parse, type, or host failure that never reached evaluation; its
  `rendered` is the whole caret report, printed verbatim.
- `Ran` — a run that compiled, matched on its `Ending`: `Settled` prints via
  `print_result`; `Raised`, `Stopped`, and `Unreturnable` (a settled value
  the wire cannot carry) print the diagnostic already rendered at the
  transport seam, verbatim (a closed-stdout stop renders to nothing);
  `Exited(code)` ends the loop (clamped through `platform::exit_byte`).
  `Stopped` is a cancellation, not a process stop: ral does not suspend
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

The same entry backs every host program the REPL holds as a value: the prompt
body, an rc startup block, and a plugin hook are *registered hooks* run as
`Program::Hook` runs ([[map/repl/plugins|plugins]]), whose registered policy
decides capture, terminal authority, and whether the run is taken aside. A
hook run is no ready boundary, and its fault comes back labelled with the
hook's name (`plugin 'p' hook 'h'`, `prompt`) by the engine; an `exit` from a
hook is ignored.

## The host

`host.rs::ReplHost` is the REPL's `Host`, and the one place every hook
dispatch goes through (`dispatch`, `run_hook` → `HookResult {outcome,
captured, ctx}`, the `Outcome` one of `Settled(value)`, `Exited`,
`Faulted(text)`, `Walled(text)`, the text trimmed and labelled once there).

- `enquire` answers the three classes `enquiry.rs` types once for both ends:
  `` `repl-editor `` (an `EditorOp` — `get`, `set`, `set-lbuffer`, `insert`,
  `push`, `accept`, `ghost`, `highlight`, `history`, `state-get`,
  `state-set` — applied to the editor context installed for the dispatch in
  flight, refused in its own words when none is), `` `repl-plugin ``
  (`loaded` with a manifest, `unloaded` with a name — told to the
  [[map/repl/plugins|plugin runtime]], which may refuse) and
  `` `repl-clipboard `` (a `ClipboardReq`, copied by the host).
- `surface` and the `DeferredSink` impl render what the engine surfaces: a
  `` `watch `` line (`ral/src/surface.rs`, shared with batch) through the
  frontend's printer above the prompt, a `` `notice `` dim, an
  `` `observed `` observation skipped by choice (the ral front-ends render no
  audit), and anything else dropped with a note naming its class.
- `prompt_fault` prints a render's faults, joined, once while they repeat.

## The selectable frontend

The loop drives a boxed `Frontend`, chosen after boot from the rc's
`Surface`:

- `Surface::Minimal` — the canonical-stdin editor.
- `Surface::Readline` (the default) — the rustyline editor.

`create_frontend` resolves it: the capability gate forces the minimal editor
on a dumb terminal whatever was asked, otherwise the surface preference
decides. The preference is set by the `--surface` flag (CLI wins)
or the rc `surface:` key. The two implementations, the `Frontend` trait,
and completion live in
[[map/repl/frontend|frontend]].

## Prompt, rc, theme

- `prompt.rs` — `render` dispatches the registered `Session/"prompt"` hook,
  registered with capture: its return value is the prompt, a returned unit
  falls back to its captured stdout. The prompt body reads `user` and `cwd`,
  the ambient builtins, directly. Plugin `prompt` hooks
  fold over the result, each a dispatch registered with capture and read
  through the same `prompt_text`; the terminal title is written separately.
  A failing prompt — one whose value cannot cross included — falls back to
  the default `❯ ` (a plugin's, to the prompt so far); the faults of one
  render print together through `prompt_fault`, when they differ from the last
  render's, and the session survives so the user can rebind it.
- `config.rs` — the rc's thirteen keys (`env`, `prompt` — registered as the
  `Session/"prompt"` hook — `bindings`, `aliases`, `edit-mode`, `bell`,
  `surface`, `recursion-limit`, `plugins`, `startup`, `theme`, `umask`, `limits`) are declared
  once, in `Form::Rc`'s table, and `RcPlan::decode` judges them all engine-side,
  inside the boot builtin, before `RcPlan::apply` commits to the shell and the
  `RcSettings` it answers; a refused rc has changed nothing. The first-run
  skeleton is written with `create_new`, so a concurrent rc is never truncated. A
  wrong type at one of the scalar keys is the table's static failure, above;
  `decode` catches those keys' further shape rules (e.g.
  `edit-mode` must be `'emacs'`/`'vi'`, not just a `String`).
- `theme.rs` — `OutputTheme` (the `value-prefix`, default `"=> "`, and an
  optional `value-color`, default yellow) governs value rendering;
  held by `Session` and passed to `step`; an unknown theme key refuses the rc.
- `errfmt.rs` — the REPL-styled plugin notices (the breaker's disable notice,
  a plugin warning), beside core's full ariadne renderer, and
  `report_failed_load`, the one printer for a failed rc, profile, or plugin
  load: the report, then a `ral: WHAT: …` line saying how far it got.
- `cursor.rs` — the zsh-style partial-line marker, written before each
  rustyline prompt. On Unix it is `PROMPT_SP`'s fill-and-erase — `%`, then
  `width − 1` spaces, then `\r\x1b[K` — which leaves the `%` only when the
  fill wrapped off a partial line, and asks the terminal nothing, so keys
  typed while a command ran reach the editor intact. On Windows the console
  API, which likewise reads no input, reports the cursor column directly.
