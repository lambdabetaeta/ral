---
status: active
generated_at_commit: f1b0a4a4
verified_at_commit: f1b0a4a4
verified_at_date: 2026-11-07
anchors: [Confined, supervise, start_file, start_tool, tool_child, begin, Dawn, Unbegun, outlive_group_signals, die_with, Witness, Running, Testimony, serve_warrant]
---

# The trampoline is a supervisor

**On Linux the `ral --warrant` trampoline, inside the bwrap envelope, spawns
the program as its child, waits for it, and reports its exact wait status; on
macOS it still becomes the program.** A confined program's reported ending is
therefore what happened to it, on both platforms, and a confined program that
cannot start is `not-found` or `not-runnable`, as an unconfined one is, never
`exited 127` or `exited 126`.

## Why

- **bwrap folds the payload's death into an exit status.** A payload killed by
  signal `n` is exit `128 + n` to the parent, so a confined `SIGSEGV` and
  `exit 139` were the same ending.
- **The trampoline's `execve` has no after.** Becoming the program leaves no
  process to say how it ended, and none to say that it could not start; the
  trampoline printed a word on stderr and exited 127 or 126, which the parent
  read as the program's own status.
- **A process that waits can say both.** Spawning the program, the trampoline
  learns the `execve`'s `errno` itself, and waiting it learns the exact status.
  One channel carries both to the parent: the report
  ([[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]).
- **macOS has no envelope to fold anything.** There the program replaces the
  trampoline and keeps its pid; the wait status is exact and the report need
  say only `unstarted` or, by its writer closing at the `execve`, that it
  started.

## The decision

- **The role is the same binary.** No new crate and no sibling helper: the
  supervisor is `ral --warrant` as before
  ([[invariants/single-binary|single-binary]]).
- **A bundled tool is forked, never run in-process under the supervisor.** A
  tool may exec (`env`), which would close the report untold. The supervisor
  never execs ([[invariants/a-supervisor-never-execs|a-supervisor-never-execs]]).
- **The ending is amended where the child is reaped.** `WaitOutcome::Enveloped`
  and its consumers' special cases are gone; the `ChildHandle`'s `Testimony`
  amends the outcome before any consumer sees it
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
- **A launch's failures are blamed by position.** The trampoline's records are
  `refused`, `unstarted(errno)`, `started` and `ended`, mapping onto
  `Unstarted::Refused` and `Unstarted::Program`; the trampoline's own fork or
  setup failing is `refused`, since `unstarted` is a file's `execve` alone
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).

## Costs

- **One more process per confined command on Linux**, a single-threaded
  supervisor beside the program, in the envelope's session.
- **A supervisor killed by a signal leaves no word**, which the parent reads as
  the death it was (`128 + n` with no record); the program, set to die with its
  supervisor, goes with it. The supervisor sets no death signal of its own: a
  kept launch ties it to ral through bwrap's `--die-with-parent`, a surrendered
  one by design does not.
- **A supervisor is a place a program can stop.** A program that stops itself
  can stop its supervisor before it says `started`; the parent's wait for that
  word is a cancellable one
  ([[invariants/a-first-word-is-waited-for-under-the-scope|a-first-word-is-waited-for-under-the-scope]]).

## Where

`core/src/authority/sandbox/warrant/trampoline.rs` (`serve_warrant`,
`Confined::run`, `start_file`, `start_tool`, `supervise`, `tool_child`,
`begin`, `outlive_group_signals`, `die_with`), `core/src/authority/sandbox/launch.rs` (`issue`),
`core/src/process/report.rs` (`Testimony`, `Witness`, `Running`, `Record`),
`core/src/process/fork.rs` (`fork`, `reap`),
`core/src/process/child.rs` (`ChildHandle::into_watch`). The narrative is
[[internals/an-external-end-to-end|an-external-end-to-end]] and
[[internals/confined-launch|confined-launch]].

**Amended 2026-11-07.** On Linux the trampoline says `started` only after its
child has begun. The child reports on a close-on-exec pipe (`Dawn`) whether its
own setup, `die_with` and for a bundled tool the signal reset, succeeded; a
failed fork, a failed setup or a child dead before it began is `refused`
(`Unbegun::Ours`), and only the program's own `execve` error is `unstarted`
(`Unbegun::Program`). A launch that is not the program's fault is never reported
as the program's ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).
