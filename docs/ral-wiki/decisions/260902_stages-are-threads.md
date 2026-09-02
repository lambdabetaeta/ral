---
status: active
---

# Stages are threads

**A ral-written pipeline stage is an OS thread running its own CEK machine
over a cloned `Shell`, wired to its neighbours by the same kernel pipes as
before.** Only an external command is a process; every external in a pipeline,
however deeply nested, still lives in one process group the parent ral process
is not a member of. The re-exec'd pipeline-stage helper — the
`ChildEvalRequest`/`ChildEvalResponse` frame pair, the wire-scrubbing it
forced, and the boot-per-stage cost — is deleted.

## Decision

- **`StageLaunch::Thread` replaces `StageLaunch::HelperEval`.** A stage thread
  runs `machine::evaluate` over a closure captured at resolve time, its own
  stack empty by construction: no frame crosses into it, exactly as none
  crossed the wire before. `direct_spawnable` loses the terminal-ownership
  condition that used to force a tty-owning pipeline's external stages through
  the helper; the foreground is claimed before any stage spawns (below), so a
  direct external no longer needs a helper in front of it to hand off the
  terminal correctly.
- **One anchor, on every platform, alive for the pipeline's whole life, and
  the group's witness.** `PipelineGroup::prepare` spawns
  `ral --ral-pipeline-anchor` — stdin a pipe the parent holds the write end
  of, stdout a pipe the parent polls, the default `SIGTSTP` disposition kept
  — before any stage exists, so a later stage's `setpgid` join always has a
  live target. Every termination signal is caught and reported as its number
  on stdout rather than obeyed. The shell is not a member of the pgid, so
  while the terminal is lent to the group the shell never hears the tty's
  Ctrl-C or Ctrl-Z: the collector reads both off the anchor
  (`PipelineGroup::witness`) — a stop parks the pipeline, a signal cancels
  its stages with the cause the shell's own handler would apply. Without
  this an all-ral foreground pipeline could not be interrupted at all, the
  anchor being the only process the kernel can reach. The SIGINT relay
  installs right after `prepare`: the anchor is a real, immune member, so a
  signal is never forwarded to a child-less pgid. The foreground is claimed
  in `PipelineBuild::new`, before any stage runs — collapsing the old
  post-launch frame gate, which no longer has a job.
- **A stage's `Mooring` carries the park, not its `LaunchRole`.**
  `StagePark { gate: Arc<StageGate>, stop: Arc<StageStop> }` travels on
  `Mooring::park: Option<StagePark>`. A `spawn` worker started inside a stage
  joins the stage's process group (`LaunchRole::PipelineStage(pgid)`) but
  mints a fresh `Mooring` with no park, so a detached worker is no more parked
  by Ctrl-Z than it is cancelled by Ctrl-C — the same law that already held
  for cancellation now holds for parking.
- **A pipeline launched inside a stage joins the outer group and never acts as
  an owner.** `PipelineGroup::joining(pgid)` installs no anchor, claims no
  foreground, and may not signal or kill the group — only the owning,
  top-level group does that. Its collector never parks, signals, or escapes:
  on a `Stopped` probe it writes the signal into its own stage's `StageStop` —
  the report the owning collector reads, since that is the same slot a direct
  `Park` arm writes — and forgets the stop locally,
  meeting the owner's pause at the `process::check` heading its next pass.
  This is how a stop the anchor cannot see — a single-member `SIGSTOP`, a
  `SIGTTIN` under `bg` — still reaches the owner, one collector per nesting
  level.
- **A `Source` is borrowed by duplication, never taken.** `Source::reader`
  hands every consumer — a builtin, an external's stdin, a nested stage — a
  `dup(2)`, exactly as the helper's shared fd 0 did, so a stage's stdin
  outlives every command that reads it and its wake always has a referent. A
  stage thread's stdin is never `Terminal`: reading a tty from a thread in the
  shell process would raise `SIGTTIN` against a foreground that belongs to the
  externals' group, so a source with no reader on a tty-owning pipeline
  resolves to `Source::Empty` instead of falling through to fd 0 (§ Risks).
- **The wake ends a stage's blocked write as well as its blocked read.**
  `Sink::Pipe` carries the stage's `Wake` and polls it between `PIPE_BUF`
  chunks; a fired wake ends the write as success, the stage's next `check`
  carrying the cause. The held duplicate of the edge's read end is kept until
  the stage is observed, as for an external: ral keeps `SIGPIPE` at its
  default disposition, and an `EPIPE` reaching a *thread* of the shell would
  kill the shell.
- **Stop and park is Unix-only and gate-mediated.** `StopPolicy` replaces the
  boolean `park_on_stop`: `KillAndReap` (batch), `Escape` (a top-level
  foreground external the REPL job table owns), or `Park(StagePark)` (anything
  inside a stage thread). A pipeline stage's stop is not its policy's to
  classify — it reaches the collector, which holds the group's role. A parked
  pipeline is a
  value, `ParkedPipeline`, holding the group with its foreground guard and
  relay already released (the anchor kept, so the pgid stays joinable), the
  gate, and the collector's unobserved state — deposited in
  `shell.session.parked` and picked up by the REPL's `Job`. `fg`/`bg`/`kill`/
  sweep drive `ParkedPipeline::resume_and_collect`/`poll`/`cancel` instead of
  `waitpid(-pgid)`, because the stage threads, not the REPL, are the ones
  waiting on the group's children now.
- **The Apple fork/CLOEXEC race is closed by one lock with one fork door.**
  `process::spawn(cmd)` is the only place `Command::spawn` runs under the
  exclusive side of the process-wide `RwLock`; `cloexec_pipe`/
  `cloexec_socketpair` take the shared side. `.output()`/`.status()` callers
  spawn through the door and wait outside it, so a child's whole lifetime
  never holds the lock other fd creation needs.

## Superseded and unamended

Supersedes
[[decisions/260610_child-eval-unification|child-eval-unification]]: the
pipeline-stage half of the shared re-exec'd-child eval protocol is gone along
with the wire it rode. `core/src/engine_seed.rs` is not deleted — trimmed from
946 to 137 lines, it now carries only `EngineSeed`/`pack_seed`, the engine
seat's own seed wire for `hatch` ([[map/core/transport|transport]]); the
`ChildKind` axis and the `PipelineStage`/`Sandbox` split it named no longer
exist, since a pipeline stage no longer crosses a wire at all.

[[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-primitives-detached-vs-structured]]'s
"pipelines are foreground-bounded" is unchanged: a stage thread's cancel scope
is still a child of the node's foreground scope, and the pipeline still joins
before the turn returns. What that ADR named as still-cutover work — an
explicit pipeline-child scope, rather than the foreground scope threaded
straight through — is what this decision builds: `node_mooring.cancel.child()`
per stage.

## Risks accepted

- **Ctrl-Z stops ral-written code at its next machine step, not at the
  instruction the kernel interrupted.** An external still freezes instantly,
  kernel-stopped; a stage thread parks only when it next reaches
  `process::check` — a stage mid-builtin (a large `to-json`) finishes that
  call first, and a stage printing to the terminal may emit one more step's
  output before parking. A pipeline with no external at all still parks,
  through the anchor as stop witness: the anchor is a group member that keeps
  the default `SIGTSTP` disposition, so the collector's probe of it sees the
  stop even when no other member of the group could report one. Ctrl-Z on a
  top-level ral loop outside any pipeline never stopped anything and still
  does not. `docs/SPEC.md` §11.6.
- **A ral-written stage 0 reading an interactive tty sees EOF.**
  `!{ from-line } | cat` at the prompt: a thread in the shell process cannot
  read a terminal whose foreground belongs to the externals' group without
  raising `SIGTTIN` against the whole shell, so a stage-0 source with no
  reader on a tty-owning pipeline is `Source::Empty` rather than falling
  through to fd 0 — the same thing it already saw under capture. An external
  stage 0 (`cat | grep x`) is unaffected: it is a process in the foreground
  group. `docs/SPEC.md` §7.6, [[design/pipelines|pipelines]].
- **A nested launch can slip one external past a park.** A direct external
  spawned by a joining launch between that collector's last `process::check`
  and its own spawn call enters an already-stopped group and runs uninterrupted
  — a newly created member is not itself stopped, and the later `SIGCONT` is a
  no-op on it. The window is the same family as "parks at the next machine
  step" above: one `check`-to-`spawn` gap.
- **A Rust stack overflow takes the shell; a stage panic does not.** An
  overflow is not an unwind — it aborts the process, exactly as it does for a
  `spawn` worker. A panic unwinds that stage's thread alone and is caught where
  the collector joins it, surfacing as an `Error` carrying the stage's span,
  the stage's own stack having none to attribute it to, and folding like any
  other stage failure; the collector reads a stage's end off
  `JoinHandle::is_finished`, which a panic cannot skip. A stage thread gets an
  8 MiB stack and the machine's own frame cap bounds ral-level recursion.
- **The anchor survives `SIGTERM`.** `PipelineGroup::drop` reaps it through
  EOF (closing the release pipe), and a cancelled group is `SIGKILL`ed first,
  so `JobTable::cleanup`'s `SIGTERM -pgid` at REPL exit cannot end an anchor
  on its own — `cleanup` cancels every parked pipeline instead, which drops
  its group and so its anchor. There is no `kill %n` verb; `fg` then Ctrl-C
  ends a job, through the witness above.
- **Five dependencies fork outside the Apple spawn lock**, found by the
  Cargo.lock sweep and accepted as a residual risk rather than closed:

  | crate | fork | reached when |
  |---|---|---|
  | `crossterm` 0.29 | `Command::new("tput").output()` | the ioctl size query fails; a direct dependency of both `ral` and `exarch` — the live one |
  | `open` 5.4.1 | outer `Command::spawn()` (its inner `libc::fork()` runs inside a `pre_exec`, post-fork and single-threaded, so safe) | `synod` opens a URL |
  | `rfd` 0.16 | `Command::new("zenity")` | `synod`'s native file dialogs on Linux |
  | `git2` 0.21 | `sh -c <credential-helper>` | only if a git credential helper is configured |
  | `grep-cli` 0.1.12 | decompression commands (`gzip`, …) | searching a compressed file |

  None is fixable inside the lock without vendoring; `rg -n 'fork\(|Command::new'`
  against `Cargo.lock`'s resolved sources is the check to re-run when the
  dependency set moves.

## Measured

Same host, same `cargo build -p ral` on both sides, debug profile, warm, median
of 21 runs (aarch64 Linux container; the binaries differ by 1 MB):

| program | before | after |
|---|---|---|
| `return 2` (no pipeline) | 3 ms | 3 ms |
| `echo hi \| cat` | 10 ms | 9 ms |
| `echo a \| !{ echo b }` | 11 ms | 9 ms |
| three ral stages | 11 ms | 9 ms |
| four ral stages | 15 ms | 11 ms |

The shape is what the change predicts — a pipeline no longer pays a ral boot
per ral-written stage, so the saving grows with the stage count and a program
with no pipeline is untouched. The magnitude is modest because process spawn on
this host is cheap: a whole `ral` boot measures ~3 ms here, against the 10–11 ms
the plan measured on its author's host, where the same change would show
proportionally more. Quote the shape, not these absolute numbers.

Verified by hand alongside the suite: `let h = spawn { return 7 }; echo x | !{ await $h }`
yields `value: 7` — a `spawn` handle closed over by a stage, which the wire
could not carry at all; a killed producer blocked writing into a full edge
exits 0 rather than killing the shell with SIGPIPE; and `!{ yes ; exit 5 } | head -1`
exits 0, the forgiven-death rule holding for a thread as it did for a process.

See also [[design/pipelines|pipelines]],
[[internals/pipeline-execution|pipeline-execution]],
[[invariants/single-binary|single-binary]],
[[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]
(unchanged: the forgiven-death rule is stated once, above the transport that
realises a stage). `docs/SPEC.md` §7, §7.6, §11.6; `docs/RATIONALE.md`
§"The pipe is the operating system's".
