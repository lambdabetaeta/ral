---
status: active
---

# Terminal-foreground ownership, not interactivity, gates the handoff

**A shell hands the controlling terminal to a child exactly when it owns the
terminal's foreground process group — `startup_foreground` — not when it is an
interactive REPL.** `startup_foreground` is `tcgetpgrp(stdin) == getpgrp()`
probed once at process entry (`io/terminal.rs`, `probe_foreground`), false when
stdin is not a tty. It is the single predicate both the standalone-command path
and the pipeline path read to decide the `tcsetpgrp` handoff.

There are three launch regimes, and the foreground decision is the same for the
first two:

- **interactive REPL** — owns the foreground, foregrounds its children;
- **non-interactive script launched at a terminal** (`ral run-claude.ral`) —
  *also* owns the foreground, *also* must foreground its interactive children;
- **tty-less / captured / backgrounded eval** (exarch tool-eval, a piped
  `ral -c`, `ral … &`) — does not own the foreground, and must not reach for it.

Gating the handoff on `interactive` folded the second regime in with the third:
a script's interactive child (`claude`, `fzf`) was spawned into a background
process group with no `tcsetpgrp`, so its first `tcsetattr` raised **SIGTTOU**,
the child parked, and ral tore the pipeline down with `[R0001]`.

- **The bite surfaced when a non-interactive top-level external began leading
  its own group.** That change (commit `beb8d1ef`, shell-timeout tree-kill) was
  correct on its own terms — an own pgid lets a watchdog `kill(-pgid)` the whole
  subtree. But before it, such a child inherited ral's pgid and so shared ral's
  *foreground* group incidentally; the own-group spawn removed that accident and
  exposed that the handoff was never keyed to terminal ownership. Not a
  regression of a designed behaviour — the discovery of a missing predicate.
- **`startup_foreground` subsumes the tty check.** It is false whenever stdin is
  not a tty, so `want_fg` and `ForegroundGuard::try_acquire`
  (`process/signal/unix.rs`) drop the separate `interactive && startup_stdin_tty`
  conjunction for the one predicate. `resolve_terminal_plan`
  (`runtime/pipeline/resolve.rs`) selects `ForegroundExternalGroup` on the same
  basis, so the standalone and pipeline paths cannot diverge on which shells own
  the terminal.
- **Release masks SIGTTOU.** `ForegroundGuard::drop`
  (`process/signal/unix.rs`) blocks SIGTTOU while restoring ral's saved pgid
  and termios. A successful handoff necessarily makes ral a background process
  group until the child exits; the restore window is parent-local, and children
  still receive the inherited-or-default SIGTTOU disposition through
  `reset_child_signals`.
- **Parking decouples from foreground.** Only an interactive REPL has a
  [[map/repl/jobs|job table]] to `fg` a parked job, so `park_on_stop =
  want_fg && interactive` (`runtime/command/foreground.rs`). A foreground child
  of a *non-interactive* script is kill-and-reaped on `SIGTSTP`/`SIGSTOP`, never
  parked — there is no session to resume it, and parking would strand it holding
  the terminal.
- **The third regime still leads its own group.** `own_group_when_background =
  may_foreground() && !interactive` is unchanged: a tty-less top-level eval
  spawns `NewLeader` for cancel tree-kill even though it never foregrounds.
- **Windows is untouched.** Consoles are shared between attached processes and
  there is no `tcsetpgrp`, so `startup_foreground` is "console attached" and only
  feeds the Job-Object / capability decisions; the pipeline plan still never
  selects `ForegroundExternalGroup` there.

See [[internals/pipeline-execution|pipeline-execution]],
[[map/core/io-process|io-process]], [[map/core/runtime|runtime]].
