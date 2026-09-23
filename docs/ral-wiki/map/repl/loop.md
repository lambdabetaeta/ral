---
generated_at_commit: 3e43ce37
generated_at_date: 2026-09-23
covers_paths: [ral/src/repl.rs, ral/src/repl/session.rs, ral/src/repl/session/, ral/src/repl/exec.rs, ral/src/repl/host.rs, ral/src/repl/enquiry.rs, ral/src/repl/prompt.rs, ral/src/repl/config.rs, ral/src/repl/config/, ral/src/repl/theme.rs, ral/src/repl/errfmt.rs, ral/src/repl/cursor.rs, ral/src/repl/worksheet.rs, ral/src/boot_door.rs, ral/src/surface.rs]
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
queued for re-edit, the probed terminal, and the exit code. On a
`structural` build it also owns the `Worksheet` (`worksheet.rs`) — the
retained binding-edge / effect-verdict model the structural surface
projects, its effect verdict read through the `bind-effects` probe. There is
no job table: ral does not suspend
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

- `Session::boot` is two-staged. Stage one is `IdentityTransport::boot` with
  `startup::engine::INSTALLERS` and an `Attach` whose `config` is
  `ReplConfig {login}`; the `repl` recipe does what precedes rc (below). A
  refusal prints its sentence and exits 2. Stage two is the host's first
  dispatch: `Program::Hook` on the `Session "boot"` hook, the `_ral-boot`
  door, applied to `Boot {login, no_rc, recursion_limit, capabilities}`
  (`ral/src/boot_door.rs`). `boot_door::settle` reads its `Ending`: `Settled`
  carries the rc's `RcSettings`, a `--capabilities` failure is `Raised`
  status 2, a profile's `exit N` is `Exited(N)`, and either ends the session
  there. The host then applies `--surface` over the rc's choice, sets the
  output theme, and dispatches `Session "startup"` if the rc registered one.
- `run` loops `iterate` until it returns `Flow::Break` — a frontend `Eof`, an
  `exit`, or a severed transport.
- `iterate` is one cycle: break if the `session-ended` reading names a cause
  (SIGTERM/SIGHUP or Ctrl-\ cancelled the durable root — cancellation is
  one-way, so exit with the cause's code rather than deal an unusable
  prompt), `process::clear` any residual interrupt, write the terminal title
  from the `cwd` reading, render the prompt, `read`, and act on the `Read`.
  `Read::Line` adds to history and evaluates; `Read::Edit` becomes next
  iteration's `pending`; `Read::Interrupt` clears the signal and sends
  `Control::Interrupt`; `Read::Eof` breaks. `read` is handed the transport,
  the prompt, the pending buffer, and (structural) the worksheet.
- `eval` runs one trimmed line through `exec::step`, recording an `exit` code
  so `run` breaks cleanly.
- Teardown — the `workers` reading, transport detach, history flush, a
  one-time notice naming any worker still running
  (`host::teardown_notice`, over `WorkerRow`s), and
  `ral_core::sandbox::teardown_session` (deletes the session's
  per-projection AppContainer profiles on Windows; a no-op elsewhere) — lives
  in `Drop for Session`, so it covers a panic unwinding through the owned
  `Session` as well as the orderly exit.

`session/boot.rs` is the process-level setup that precedes the engine and
the frontend the rc settles on after it: `setup_signals` (the whole Unix
disposition table in one place — SIGINT `interrupt_handler`, which raises the
foreground interrupt and nothing else, SIGQUIT root-abort, SIGTERM/SIGHUP term
handler, SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE ignore), `claim_terminal` (run first,
while SIGTTIN still has its default disposition: it parks the shell on SIGTTIN
until it is foregrounded before `tcsetpgrp`, so `ral &` does not seize the
terminal from a parent shell's current job), `setup_panic_hook` (restore the
pre-raw terminal state — termios on Unix, console mode on Windows — then write
a crash log, to a state-dir path resolved at install time so a changed `HOME`
cannot redirect it), and `create_frontend`.

## The boot, engine-side

The `repl` installer's recipe (`startup::engine::boot_repl`) boots the REPL
surface — the batch surface plus the [[map/repl/plugins|`_ed-*` doors and
the plugin load doors]] — through `boot_shell`, so the typechecker and the
runtime see one surface by construction; then exit hints, the
interactive flag, `$TERMINAL`, the login umask, and the boot door's
registration.

The boot door (`_ral-boot`, one-shot: it unregisters its own hook) does the
rest in a run, since startup files evaluate ral: `source_startup_files`
(`config/source.rs` — login profiles, then the rc), the CLI's
`--recursion-limit` after the rc, `--capabilities` through
`capability::apply_session_profiles` (the user's ceiling is the last word,
and a session frame it pushes survives the door's run), and
`install_default_prompt` (register the default `Session/"prompt"` hook,
`{ return "❯ " }`, only when no rc `prompt:` key registered one). It answers
the rc's `RcSettings {edit_mode, bell, surface, theme, startup}` — the only
part of the rc the host needs; everything else the rc configures lands on the
engine's shell.

Each startup file has one contract on its return value: a profile is sourced
for its effects and returns `()`; the rc returns a configuration record. An
`exit` in either ends the session with its status; any other failure is
reported and the boot goes on. The rc goes through `compile_and_typecheck`
against the live session, against the `FileId` `evaluate_checked` registers
its text with, so an alias or function it defines keeps naming the rc for the
whole session. It compiles under a **return contract**
(`ral_core::typecheck::ReturnContract`): `contract::declared(Form::Rc)`, whose
closed keyset the rc's own returned *row* is held to — the same rule
`within`/`grant` options get, extended to a program's own return value. It is
the inferred row and not the syntax that produced it, so a key misspelled
inside a spread is caught with one written out; a return carrying no row (a
`Map`) meets the same keyset at `apply_rc_config` instead. Both failing
`CompileError` arms — `Parse` and `Types`, a broken contract among the
latter — are *reported and skipped*: the file has no runnable annotation,
while the boot always survives
([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]), the
whole rc unapplied rather than only the offending key. A *computed* rc return
(a bound variable, a call) is invisible to the contract and stays on
`apply_rc_config`'s own per-key runtime check, which meets the same keyset
and field types before applying anything and refuses the whole rc on an
unknown key or a malformed value alike. An rc `startup` block registers as
the `Session/"startup"` hook, and the host dispatches it as a hook run under
`Denied` terminal authority — a fresh frame whose `let`s do not leak.

## Every evaluation is a dispatch

**Each line, prompt, and hook is one `Run` through `dispatch_to_report` with
the REPL host, and the engine's door compiles, typechecks, and runs it against
the live session** ([[internals/a-turn-end-to-end|a run, end to end]]).

`exec.rs::step` is the per-line entry. `line_run` builds the `Run` —
`Program::Source(trimmed)`, `script_name: "<stdin>"`, `GrantStack::root()`,
no wall, `RunIo::Inherit`, `RequestedTerminalAccess::Leased`,
`RunStdin::Inherit` — and `ReplHost::dispatch` sends it. Around it the
lifecycle hooks fire as dispatches of their own (`plugin::fire`): `pre-exec`
with `{src}` before; after, `chpwd` with `{old, new}` if the `last-chpwd`
reading moved past the `seq` seen before the line, then `post-exec` with
`{src, status}` — for every `pre-exec`, a static failure's status included.
An `Err(Severed)` prints the cause and ends the session.

It matches the one flat `Report`:

- `Static` — a parse, type, or host failure that never reached evaluation; its
  `rendered` is the whole caret report, printed verbatim.
- `Ran` — a run that compiled, matched on its `Ending`: `Settled` prints via
  `print_result` (and, on a `structural` build, records the bind into the
  worksheet); `Raised`, `Walled`, and `Unreturnable` (a settled value the
  wire cannot carry) print the diagnostic already rendered at the transport
  seam; `Exited(code)` ends the loop (clamped through
  `platform::exit_byte`). There is no `Stopped` arm
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

The same door backs every host program the REPL holds as a value: the prompt
body, an rc startup block, and a plugin hook are *registered hooks* run as
`Program::Hook` runs ([[map/repl/plugins|plugins]]), whose registered policy
decides capture, terminal authority, and whether the run is taken aside. A
hook run is no ready boundary, and its fault comes back labelled with the
hook's name (`plugin 'p' hook 'h'`, `prompt`) by the engine; an `exit` from a
hook is ignored.

## The host

`host.rs::ReplHost` is the REPL's `Host`, and the one door every hook
dispatch goes through (`dispatch`, `run_hook` → `HookResult {value,
captured, fault, walled, ctx}`).

- `enquire` answers the two classes `enquiry.rs` types once for both ends:
  `` `repl-editor `` (an `EditorOp` — `get`, `set`, `set-lbuffer`, `insert`,
  `push`, `accept`, `ghost`, `highlight`, `history`, `state-get`,
  `state-set` — applied to the editor context installed for the dispatch in
  flight, refused in its own words when none is) and `` `repl-plugin ``
  (`loaded` with a manifest, `unloaded` with a name — told to the
  [[map/repl/plugins|plugin runtime]], which may refuse).
- `surface` and the `DeferredSink` impl render what the engine surfaces: a
  `` `watch `` line (`ral/src/surface.rs`, shared with batch) through the
  frontend's printer above the prompt, a `` `notice `` dim, an
  `` `observed `` observation skipped by choice (the ral front-ends render no
  audit), and anything else dropped with a note naming its class.
- `prompt_fault` prints a broken prompt's diagnostic once while it repeats.

## The selectable frontend

The loop drives a boxed `Frontend`, chosen after boot from the rc's
`Surface`:

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

- `prompt.rs` — `render` dispatches the registered `Session/"prompt"` hook,
  registered with capture: its return value is the prompt, a returned unit
  falls back to its captured stdout. USER and CWD are ambient
  pseudo-variables the prompt body reads directly. Plugin `prompt` hooks
  fold over the result, each a dispatch; the terminal title is written
  separately. A failing prompt — one whose value cannot cross included —
  falls back to the default `❯ `, printing its diagnostic when it differs
  from the last one printed, and the session survives so the user can rebind
  it.
- `config.rs` — the rc's eleven keys (`env`, `prompt` — registered as the
  `Session/"prompt"` hook — `bindings`, `aliases`, `edit_mode`, `bell`,
  `surface`, `recursion_limit`, `plugins`, `startup`, `theme`) are declared
  once, in `Form::Rc`'s table, and `apply_rc_key` applies each engine-side,
  inside the boot door, to the shell or to the `RcSettings` it answers. A
  wrong type at one of the scalar keys is the table's static failure, above;
  the per-key runtime check catches those keys' further shape rules (e.g.
  `edit_mode` must be `'emacs'`/`'vi'`, not just a `String`).
- `theme.rs` — `OutputTheme` (the `value_prefix`, default `"=> "`, and an
  optional `value_color`, default yellow) governs value rendering;
  process-global behind an `RwLock`, set once from the boot's answer.
- `errfmt.rs` — the REPL-styled plugin notices (the breaker's disable notice,
  a plugin warning), beside core's full ariadne renderer.
- `cursor.rs` — Unix cursor-column query for the zsh-style partial-line
  marker.
