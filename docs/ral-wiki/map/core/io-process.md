---
generated_at_commit: 478c977
generated_at_date: 2026-06-18
covers_paths: [core/src/io/, core/src/io.rs, core/src/process/, core/src/process.rs, core/src/stream.rs]
---

# Map: core / IO, process & stream

The byte plumbing under the [[map/core/evaluator|evaluator]]'s pipelines and
external commands.

## IO — `core/src/io/`

`io.rs` holds `Io`, the per-`Shell` bundle (stdin / stdout / stderr /
interactive / terminal / job_control / capture state) and `JobControl`, the
foreground-eligibility token that distinguishes the orchestrator
(`JobControl::top_level`) from pipeline-local children
(`JobControl::pipeline_child`). Submodules: `source.rs` (`Source`, a stage's byte
input), `sink.rs` (`Sink`, byte output plus child stdio routing and the
`ByteBuffer` capture primitive), `terminal.rs` (`TerminalState` — cached
isatty / NO_COLOR / mode bits, plus `startup_foreground`, whether ral owned the
controlling terminal's foreground at entry — the predicate gating the
`tcsetpgrp` handoff, [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
`Io::inherit_from` / `return_to` move the read-once stdin between parent and
child shells.

## Process — `core/src/process/`

- `outcome.rs` — `Signal`, `WaitOutcome`, and the user-facing `SpawnFailure` /
  `CommandFailure` the evaluator surfaces.
- `signal.rs` — the global termination flag (`check` / `clear` /
  `is_interrupted`) polled cooperatively in hot loops
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]); `interrupt`
  sets that flag to exactly 1 without escalating toward the third-signal `_exit`
  — the non-escalating unwind a raw-mode frontend drives on Esc
  ([[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]);
  the cause-bearing `CancelScope` tree (`DurableRoot` / `ForegroundScope`,
  `CancelCause`) for structured-concurrency cancellation, with the per-turn
  signal-reachable slots (`request_foreground_cancel` / `request_root_cancel`)
  that let a handler or TUI thread cancel a scope it cannot hold; `Pgid` /
  `PgidPolicy` / `ChildHandle` and the platform `spawn_with_pgid` family for
  process-group placement. Unix `ForegroundGuard` snapshots and restores tty
  foreground / termios, blocking SIGTTOU for the parent-only restore window; unix
  `interrupt_foreground_child` re-sends raw-mode Esc/Ctrl-C to a foreground
  external group, `sigint_relay` fans SIGINT to active external pgids, and
  `sigquit_handler` is the Ctrl-`\` root abort. Platform handlers live in
  `signal/unix.rs` and `signal/windows.rs`. The whole stop-work flow is narrated
  in [[internals/cancellation|cancellation]].

## Stream — `core/src/stream.rs`

Shared label vocabulary for the lazy Stream protocol: runtime variant labels
`more` / `done` and the `head` / `tail` payload fields, with the type-row
spellings (`` `more `` / `` `done ``) kept beside them so runtime and
[[map/core/typecheck|typechecker]] recognition cannot drift. `docs/SPEC.md` §13
covers Stream semantics.
