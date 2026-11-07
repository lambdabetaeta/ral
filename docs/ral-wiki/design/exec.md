---
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
---

# Exec: the one effect

This page is about the only effect operation in ral: running an external
program. It states the principle, then gives the whole account of the
operation: which heads denote it, what it reads, the modes in which it runs,
what it returns, and every way it can end.

**Running an external program is an algebraic effect operation, and the
operating system is its default handler.** A head such as `git` names an
operation. Performing it is one step into the world that returns once, with
`()` and some bytes written, or with a failure. What that step *means* is
supplied separately: by the kernel, or by a handler the program installs over
it. The separation of operation from interpretation is the one move from which
the rest of ral's dynamic design follows.

## The principle

[[design/cbpv|Call-by-push-value]] divides terms into values, which *are*, and
computations, which *do* ([[related/call-by-push-value|Levy]]). The principle
says what the doing consists of.

- **The effect signature has one operation, `exec`.** A bare name that no
  binding or handler claims is an open name for it
  ([[design/name-resolution|name-resolution]]).
- **The rest of the language does not touch the world on its own account.**
  `$name` dereferences a binding; a value never runs; bindings, the prelude
  and most builtins compute values ([[design/builtins|builtins]]).
- **Not every kernel call is an operation.** Structured queries (`list-dir`,
  `file-info`, `glob`), redirects and ral's own writes to its output reach the
  kernel, but they are primitives of the shell, not open names: no handler can
  reinterpret them, and the queries return records rather than bytes. The
  effect boundary is drawn at the external *process*, the `exec`/`wait`
  surface that defines a shell.
- **Handlers are a layer on top, not part of the principle.** An external is
  an operation whether or not any frame reinterprets it. When one does, the
  handler is deep, self-masking and tail-resumptive: its returned value is
  substituted for the operation and the body resumes exactly once
  ([[design/effects-handlers|effects-handlers]],
  [[internals/handler-dispatch|handler-dispatch]]). There is no first-class
  `resume`. In the Plotkin–Power presentation every operation carries a
  continuation parameter; the precise claim is that ral never *reifies* it
  ([[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]]).
  Multi-shot resumption earns its keep for pure effects such as
  nondeterminism; re-running the rest of a session against a second answer
  from a world already changed is incoherent.

The consequences are recorded on their own pages:

- `grant` attenuates which programs may run and what they may reach
  ([[design/grant|grant]]);
- the check sits where the operation is performed, in-process for what ral
  dispatches and in the kernel for what a child does
  ([[design/two-enforcers|two-enforcers]]);
- authority is dynamically scoped because a caller cannot know lexically
  which operations a callee performs ([[design/scoping|scoping]]);
- failure is the operation's exceptional outcome, not a `Bool`
  ([[design/failure|failure]]);
- the audit trail records each performance, beside ral's own reads and
  writes ([[design/audit|audit]]);
- exarch's sandbox is a grant over this operation
  ([[design/exarch-architecture|exarch-architecture]]).

[[related/system-c|System C]] reads the same separation through capabilities
as values; ral keeps authority dynamic.

## The operation, informally

Write `Command` for `F^w Unit`, the producer grade of something that writes
([[design/capture|capture]]). The operation takes a program and an argument
list and runs in an ambient context:

```text
exec : Program × List String → Command
context: cwd, env overrides, stdin, stdout, stderr, grant stack,
         cancellation scope, terminal access

⟨ exec p ws | κ ⟩ → ⟨ return () | κ ⟩                       exit status 0
                  → ⟨ ↯(process [program: p, failure: f], prov κ) | κ ⟩
                  → ⟨ ↯(raised m, prov κ) | κ ⟩             refused before running
                  → ⟨ ⇑ stop(c) | κ ⟩                        cancelled by cause c
```

A *failure* `↯` is an error record that `try` can catch; a *stop* `⇑` is an
escape that no `try` sees ([[design/endings|endings]]). The bytes the program
writes are not the result: they go to the sinks in the context.

## Which heads denote it

**A head's spelling decides whether it is looked up or goes straight to the
operation.** The classification is in `core/src/shell/exec/command.rs`
(`Head`, `Target`) and `core/src/shell/exec/dispatch.rs` (`resolve`,
`resolve_command_word`); `docs/SPEC.md` §6.2–§6.3 is normative.

| Head | Example | Consults | Reaches |
|---|---|---|---|
| bare name | `git status` | bindings, then the handler stack | the bundled tool of that name, else a `PATH` search |
| caret | `^git status` | nothing | the bundled tool of that name, else a `PATH` search |
| path | `./build`, `/usr/bin/env` | nothing | that file, anchored at the logical cwd |
| tilde path | `~/bin/tool` | nothing | that file under `HOME`; with no `HOME`, not found |

- **A bare name is looked up in a fixed order**: a value binding (lexical,
  session, prelude and builtins alike), a named handler, a base handler such
  as `echo`, the catch-all handler, and only then the operation. A binding
  that is not applicable is an error, not a fall-through
  ([[decisions/260930_a-binding-shadows-a-command|a-binding-shadows-a-command]]).
- **`^name` and path heads opt out of interpretation.** They skip bindings and
  the whole handler stack, base frames included, so `^echo` is the system's
  `echo`. They remain subject to the grant.
- **A bundled tool wins over a host file of the same bare name**, and `^`
  does not change that: `^ls` is the bundled `ls`. Only a path head reaches
  the host's `/bin/ls`. A bundled tool (uutils coreutils, `diff`/`cmp`, `rg`)
  is never run in-process: it is a child, `ral --ral-bundled-tool <tool>`, so
  it shares an external's pipes, failures, audit and confinement
  ([[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]],
  [[invariants/single-binary|single-binary]]).
- **`Head::shown` is the spelling every report uses**: as written, with a `~`
  head expanded. It becomes `program` in a process failure and the first
  element of an audited `argv`.
- **On Windows** a bare search appends each `%PATHEXT%` suffix, read from the
  process environment and not from `within [env: …]`; the cwd is not
  searched; `.bat` and `.cmd` files are refused at the spawn.

**A handled command is not a performance of the operation.** A named arm
receives the rendered argument list, `List String`; the catch-all receives the
name as well. The arm stands in for the command it replaces, so its type is
`List String → Command` unless a base frame fixes another
([[design/effects-handlers|effects-handlers]]). While the arm runs its frame
is masked, so a same-name call inside it reaches the next frame or the OS. No
process starts for the handled call itself, and audit records only the
externals the arm runs.

## Inputs

### The program and its arguments

- **The argument list is a list of strings, one word each.** Nothing is split,
  globbed or re-lexed, at the call or in captured output
  ([[design/words|words]],
  [[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]]).
- **Values that are not words are refused at the boundary**: a list, a map or
  record, a block, a handle, bytes. One declaration (`RefusedArg`) is read by
  the checker on concrete types and again at the spawn by `vet`, so the two
  refusals speak one language ([[invariants/exec-argv-is-words|exec-argv-is-words]]).
- **Arguments play no part in authority.** An admitted program runs with any
  arguments.

### The ambient context

`within [dir: …, env: …]` sets the context for a body's dynamic extent
(`docs/SPEC.md` §9.1–§9.3; [[design/scoping|scoping]]).

- **The cwd is ral's logical directory.** It is passed as the child's working
  directory and as `PWD`; `OLDPWD` is removed, since ral keeps none.
- **Environment overrides lie over the inherited environment.** The bare-name
  search reads the overridden `PATH`, relative entries anchored at the cwd.
  Under any active grant the dynamic-loader variables (`LD_PRELOAD`,
  `LD_AUDIT`, `LD_LIBRARY_PATH`, every `DYLD_*`) are stripped.

### Standard streams

*Sources* and *sinks* are ral's own stream values, lowered onto the child's
descriptors at the spawn (`core/src/shell/exec/command.rs`). Only three
streams exist ([[invariants/redirects-are-the-three-standard-streams|redirects-are-the-three-standard-streams]]).

| stdin | arises from |
|---|---|
| the terminal | a run that inherits it, when fd 0 was a terminal at startup and the child's group can read it |
| a pipe | the previous pipeline stage |
| a file | `< f` |
| a here-string | `<< s`, written into a pipe by a thread |
| empty (`/dev/null`) | captured runs (exarch), workers, `detach`, and a startup terminal the child's group cannot read |

**One rule decides what a child at the boundary reads** (`boundary_stdin`), and
the child's source is then wired as it stands.

- A group the child joins passes its owner's source through.
- A startup terminal survives only where the child's group can read it: ral's
  own group, or one lent the terminal.
- A group of its own that is not lent the terminal reads EOF.
- An interactive enveloped child keeps the terminal as stdin, because
  readability follows the `Inherit` that `placement` asked for.

| stdout / stderr | arises from | the child sees |
|---|---|---|
| the terminal | the default | the inherited descriptor |
| a pipe | the next stage (stdout only) | the pipe |
| a file | `>` (staged, renamed on commit), `>>`, `>~`, `2>`…, `2>&1` | the file |
| a capture buffer | `let x = cmd` (stdout only; stderr is untouched) | a pipe ral pumps |
| an audit tee | `audit { … }` (both streams, still reaching their destination) | a pipe ral pumps |
| a worker buffer | `spawn` / `watch` | a pipe ral pumps |

A pumped stream is not a terminal, so a command whose stdout is captured or
audited sees no terminal and takes the foreground only under an explicit loan,
whose body (`_ed-tui`'s `fzf`) draws on `/dev/tty` whatever its stdout.

### Authority

**A head is judged before it is looked at, and ral runs exactly the program
the judgment admits** ([[internals/exec-judgment|exec-judgment]]).

- `Head::admit` walks each candidate and judges the object it reaches, by real
  path with links followed, under the grant stack: running needs both exec and
  read authority ([[design/authority-tables|authority-tables]],
  [[internals/path-judgment|path-judgment]], `docs/SPEC.md` §12.3).
- A bare name folds over its `PATH` candidates and runs the first one admitted
  that is a program. A bundled tool is judged by name.
- The kernel then enforces a projection of the same table on the child
  (§Confined launch below; [[design/two-enforcers|two-enforcers]],
  [[internals/capability-enforcement|capability-enforcement]]).

### Cancellation scope and deadlines

- **Every performance runs under the current scope node**, and its wait reads
  that scope between slices; a cause in force starts a teardown
  ([[internals/cancellation|cancellation]]).
- **A deadline is a scope cancelled with `TimedOut`** (`process::deadline`).
  Core arms one only for a host's requested run wall, such as an exarch tool
  call's timeout. ral has no timeout form of its own. The bundled `timeout(1)`
  is an ordinary program: its exit 124 is the failure `exited 124`, not a stop.

### The terminal

On Unix a terminal has one *foreground process group*, the group that
receives keyboard signals and may read the terminal; a background reader is
stopped with `SIGTTIN`.

- **A host requests terminal access per run**, `RequestedTerminalAccess`:
  `Leased` for the REPL and batch, `Denied` for exarch. A lease exists only if
  ral owned the foreground at startup.
- **One rule fixes the process group and the handoff**, for a standalone command
  and a pipeline alike (`placement`,
  `core/src/shell/exec/command.rs`):

  | policy | when | consequence |
  |---|---|---|
  | `Join` | inside a pipeline stage | the stage's group, never lent the terminal |
  | `NewLeader` | the run holds the lease, stdout is the terminal or the run is an explicit loan, and no envelope | the terminal is lent to the child's group with `tcsetpgrp`, and taken back at its death |
  | `Inherit` | an interactive run otherwise | ral's own group |
  | `NewSession` | a non-interactive run otherwise | own session and group, so a teardown reaches the whole tree |

  A pipeline's group is led by its anchor, which takes the `NewLeader` row's
  loan for the whole group.

- **ral has no job control.** The reaper answers every stop with `SIGCONT`, so
  Ctrl-Z suspends nothing and no stopped job ever exists
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
- **Windows has no terminal handoff.** A pipeline's members share the console,
  each spawned as the root of its own console group, and are supervised as one
  Job Object.

## Modes

**Every mode performs the same operation; modes differ only in the context
they wire around it.** Each runs the same three stages, `prepare`, `start` (or
`start_detached`) and `ending` (`core/src/shell/exec/command.rs`), and
wires its own streams between the first two
([[internals/an-external-end-to-end|an-external-end-to-end]]).

- **Building a launch only describes it; starting it acts.** `prepare` waits
  on nothing a stop must end, so it can only be refused. Two exceptions touch the world: a guest jail's
  cgroup, which `JailCgroup::drop` removes if the built launch is dropped,
  and Linux's bwrap host probe, which waits on bwrap children once per
  process.
  Every step that can be stopped, Windows' ACL stamps and the wait for a
  trampoline's first word among them, runs in `start`, through
  `Launch::spawn`, under the run's scope, which the spawn reads before it
  confines and once more just before the child is made. So `start` is the
  one place a launch is stopped or fails as a program
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).
  The order of the process limit, the `pre_exec` hooks, the report socket and
  the warrant is fixed at build and unchanged by the spawn
  ([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

- **A plain command.** Dispatch admits; `prepare` vets the argv and builds the
  launch; the command wires the streams and decides the group; `start` spawns;
  it waits, joins the pumps and `ending` classifies how the child ended
  (`command::run`).
- **A pipeline stage.** Only an external is a process; every other stage is a
  thread of ral's own process
  ([[decisions/260902_stages-are-threads|stages-are-threads]],
  [[design/pipelines|pipelines]],
  [[internals/pipeline-execution|pipeline-execution]], `docs/SPEC.md` §7.6).
  - An external stage with no redirect and no byte audit is launched
    directly; otherwise a thread hosts it as a plain command.
  - All externals of a pipeline share one process group, led by an anchor
    process that ral re-execs; ral itself is not a member. A foreground
    pipeline lends the terminal to that group.
  - A Linux stage under an envelope leads its own group.
- **A captured command.** `let x = cmd` binds what `cmd` writes. The bind rule
  inserts `capture M to d. decode d` at `F^w Unit`, the one coercion
  ([[design/capture|capture]],
  [[internals/output-capture-and-detachment|output-capture-and-detachment]]).
  Decoding drops one trailing newline and demands UTF-8. The buffer holds at
  most 16 MiB, beyond which the capture fails.
- **A redirected command.** The child writes or reads the file itself; no
  bytes pass through ral. A staged `>` commits when the frame settles and is
  audited as a `write`.
- **Under `audit`.** Both streams are teed into 1 MiB buffers, and every
  pipeline stage becomes a thread so that its external is teed too. A
  redirected stream bypasses the tee; its bytes appear only in the `write`
  observation ([[design/audit|audit]], `docs/SPEC.md` §13.3).
- **Inside a worker.** `spawn { … }` runs in a child shell with empty stdin,
  buffered stdout and stderr, no terminal lease and `NewSession` groups, so no
  terminal key reaches it. `await` replays the bytes, then returns the value
  ([[design/residency|residency]], `docs/SPEC.md` §11.1).
- **Detached.** `detach` (a Unix base frame) launches with null streams and
  surrenders ownership, so the process outlives the session
  (`core/src/shell/exec/command.rs`, `docs/SPEC.md` §11.5).
- **A host hook.** The REPL's external editor runs `$VISUAL`/`$EDITOR`
  through `Shell::run_external_argv`, which skips bindings and handlers.
  Every other hook, prompt, plugin event or keybinding reaches exec only by
  evaluating ral code, and so through a plain command or a stage.

### Confined launch

**A restrictive grant confines each external at its own spawn.** It applies
when `SandboxProjection::of` finds something to enforce: a restricted
filesystem, an offline network, a withheld GPU, or a restricted exec table
where the kernel can render one.

- **macOS.** ral re-execs its own pinned binary as `ral --warrant <fd>`; the
  descriptor carries a *warrant*, the compiled Seatbelt profile and the
  program. The child enters the profile and `execve`s the program in place,
  so the program keeps the child's pid ([[internals/seatbelt-profile|seatbelt-profile]]).
- **Linux.** ral spawns `bwrap`, the *envelope*, whose payload is the same
  `ral --warrant` trampoline. It enters Landlock and seccomp, then runs as a
  *supervisor*: it spawns the program as its child, waits, and reports the
  exact wait status, since bwrap alone would fold a signal death into exit
  `128 + n` ([[internals/linux-envelope|linux-envelope]],
  [[decisions/261107_the-trampoline-is-a-supervisor|the-trampoline-is-a-supervisor]],
  [[invariants/a-supervisor-never-execs|a-supervisor-never-execs]]).
- **Both Unix backends testify.** The trampoline writes a few records on a
  close-on-exec report socket, so a confined program that cannot start is
  `not-found` or `not-runnable`, never `exited 127` or `exited 126`
  ([[invariants/a-launchs-report-is-the-trampolines-alone|a-launchs-report-is-the-trampolines-alone]]).
- **Windows.** No re-exec: the program's own `CreateProcessW` carries an
  AppContainer token whose capabilities are this launch's grants, stamped by
  the spawn, under the run's scope.
- The details, including how a denied path is kept denied, are
  [[internals/confined-launch|confined-launch]] and
  [[internals/holding-a-deny|holding-a-deny]].

## What it returns

**A successful performance returns `()`.** The exit status is not a value, and
the bytes are not the result ([[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]],
`docs/SPEC.md` §6.8).

- **Bytes become a value only at a capture or a decoder.** A `let` decodes the
  capture as a `String`; `from-json`, `from-lines` and the other `from-X`
  decoders read a byte stream into typed data ([[design/codecs|codecs]]).
- **A pipeline's value is its last stage's.** Earlier stages' values are
  discarded.
- **Audit records each performance as a `command` observation**
  (`fact::observation::Command`): `argv` with the shown name first; `status`,
  0 or the code of the failure or stop; `origin`, `external` or `detached`; a retained
  prefix of at most 1 MiB of stdout and of stderr, with their byte totals;
  and the error's wording on failure. A directly launched stage, which exists
  only without byte audit, records no bytes. The returned value is not
  recorded.

## Endings

**The operation ends in one of three ways: it returns, it fails with a
reason, or it is stopped by a cause.** A failure is data to `try`; a stop is
an escape owned by a scope ([[design/endings|endings]],
[[design/failure|failure]],
[[decisions/261105_the-error-record|the error record]], `docs/SPEC.md`
§8.5–§8.6, §8.10). A failure carries this record:

```text
Reason     = `process [program: String, failure: Failure]
           | `timed-out | `cancelled | `raised String
Failure    = `exited Int | `signaled Int | `not-found
           | `not-runnable [found: String, why: Unrunnable]
Unrunnable = `permission-denied | `directory | `not-executable
           | `missing-interpreter (Option String) | `io String
ErrorRecord = [reason, frame, site]
```

`program` is the shown head. `frame` and `site` are provenance only: the
innermost visible frame and its source position, never the subject of the
wording (`core/src/fact/error.rs`; `core/src/shell/runtime/error.rs`). The
exit code is a projection of the reason (`FailureCode`), never stored.

### The outcomes

| outcome | arises when | ending | code |
|---|---|---|---:|
| success | the program exits 0 | returns `()` | 0 |
| `exited n` | the program exits `n ≠ 0` | failure | `n` clamped into 1..255 |
| `signaled s` | a signal kills it, and no cause in force sent that signal | failure | 128 + `s` |
| `not-found` | no candidate exists, or the spawn reports `ENOENT` for a bundled tool or a file no longer at its real path | failure | 127 |
| `not-runnable … missing-interpreter` | the spawn reports `ENOENT` for a file that still exists: its `#!` interpreter (named) or loader is missing | failure | 126 |
| `not-runnable … directory` | a path head names a directory | failure | 126 |
| `not-runnable … not-executable` | a file without an execute bit, or another non-program at a path head | failure | 126 |
| `not-runnable … permission-denied` | the spawn reports `EACCES`; or a path head's walk is refused; or a confined script's interpreter is hidden by the launch's own projection (`ENOENT` or `EACCES`), with the interpreter hint | failure | 126 |
| `not-runnable … io m` | any other spawn error, or any other failure of a path head's walk | failure | 126 |
| grant refusal | the judgment denies the program | failure, `raised` | 1 |
| argument refusal | an argument is not a word | failure, `raised` | 1 |
| launch refused | ral could not start the launch: the envelope or launcher fails to exec, a step of ral's own before the `execve` fails, the trampoline refuses, or the envelope dies before the program starts | failure, `raised` | 1 |
| stopped | a death by a signal of a cause in force, or a cause the start sees before the program runs | stop | 124, 130, 131, 141, 143 |
| cut at the write | a non-final stage writes after its reader ended | stop, forgiven by its pipeline | none |

The clamp's floor is 1, so a negative Windows `NTSTATUS` never reads as
success.

### Found before the spawn, or by it

**Most failures are found by the probe, before any process exists**
(`Head::select` and `validate_argv` in `command.rs`). A *walk* is the component-by-component
resolution of a path that the judgment also reads.

- **The probe** reports absence as `not-found`; at a path head, a directory as
  `directory` and any other present non-program as `not-executable`; at a
  `PATH` candidate only a regular file lacking an execute bit counts, and
  everything else is passed over as absent. Any execute bit counts, whoever
  owns it. A path-head walk that is refused is
  `permission-denied`; one that fails in any other way but absence is `io`. Failing every `PATH` candidate, the first that was
  refused or not executable says why, else `not-found`.
- **The spawn** reports what the program's own `execve` met, and the image
  says what it means (`at_spawn`). `EACCES` is
  `permission-denied`; everything else but `ENOENT` is `io`. **`ENOENT` for a
  file still at its real path is `missing-interpreter`**, naming the `#!`
  interpreter read when the launch was built, or the loader a native image
  needs; for a bundled tool or a file gone, it is `not-found`. A script with no
  `#!` line (`ENOEXEC`) is `io`; an execute bit for another user or a `noexec`
  mount is `EACCES`.
- **Only a permission refusal is `permission-denied`.** A path head's walk that
  fails otherwise reads as itself, `io` with the OS's words, and a `PATH`
  candidate's walk failure is absence.
- **A confined script whose interpreter the launch's own projection hides is
  `permission-denied` with the interpreter hint**, whatever `errno` the kernel
  chose: Seatbelt refuses the exec (`EACCES`), bwrap hides the interpreter
  (`ENOENT`), whether an exclusion, the exec table or the read set withholds
  it. The hint comes from this launch's own projection, asked only once the
  start has failed, about the interpreter read with the image; with no projection (the guest jail) there is no hint and a hidden
  interpreter reads as missing.
- **A start is the program's failure only if the program's `execve` failed.**
  Under confinement the `execve` happens in the trampoline, which reports it as
  `unstarted(errno)`; a launcher or envelope that fails to exec, a payload
  process group that cannot be named, the trampoline's `refused`, an envelope
  that died before reporting, and a step of ral's own before the `execve` are
  `raised` refusals, never a failure of the program
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]],
  [[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]).
- **A grant refusal is `raised` text, not a process failure**, because the
  program was never looked at: `command 'curl' denied by active grant: …`
  (`Shell::exec_refused`, `authority/guard`). The refusal is recorded as an
  exec `check` in audit and reveals nothing about the file. A refusal precedes
  absence: a denied path that names nothing is still denied.

### Stops: cancellation and signals

**A death is a cancellation exactly when it is by a signal of a cause in
force** (`WaitOutcome::classify`, `core/src/process/outcome.rs`;
[[internals/cancellation|cancellation]];
[[decisions/260706_signals-are-causes|signals-are-causes]]).

- A cause reaches a child as a cause-directed teardown, sent to its group
  when the child leads one and to its pid otherwise:

  | cause | raised by | teardown | code |
  |---|---|---|---:|
  | `interrupted` | Ctrl-C | `SIGINT`, 500 ms, `SIGKILL` | 130 |
  | `cancelled` | `cancel`, a `race` loser | `SIGTERM`, 500 ms, `SIGKILL` | 143 |
  | `timed-out` | a deadline | `SIGTERM`, 500 ms, `SIGKILL` | 124 |
  | `terminated` | `SIGTERM`, `SIGHUP` to ral | `SIGTERM`, 500 ms, `SIGKILL` | 143 |
  | `aborted` | Ctrl-\ | `SIGKILL` | 131 |
  | `reader-gone` | the pipeline collector | `SIGKILL` | 141 |

- **A death by any other signal is the program's own** `signaled s`, a
  `SIGSEGV` during a teardown included. **An exit is never a signal**:
  `exit 130` is the program's choice. A confined program's end is its
  supervisor's exact word, so a confined `SIGSEGV` is `signaled 11` and an
  `exit 139` is `exited 139`
  ([[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
- **Only a terminal ral lent can turn a key into a cancellation.** While a
  command or pipeline holds the terminal, the kernel delivers Ctrl-C to it,
  not to ral; ral hears the key back when it reclaims the terminal and strikes
  the run ([[decisions/260924_the-lent-terminal-returns-the-gesture|the-lent-terminal-returns-the-gesture]]).
  Without a loan (exarch, a capture, a worker), a `SIGINT` death is
  `signaled 2`, which `try` catches.
- **Windows** has no signals. A teardown opens with Ctrl-Break to each console
  group ral owns and ends with `TerminateProcess` or `TerminateJobObject`
  using one fixed exit code, `KILL_EXIT_CODE`. A process that ends with that
  code or with `STATUS_CONTROL_C_EXIT` while a cause is in force is stopped by
  that cause. A Ctrl-C exit with no cause in force is `exited`, worded
  `ended by Ctrl-C`. Ctrl-C at ral's console raises the interrupt.
- **A worker's boundary converts its own stops.** A `cancelled` or `timed-out`
  stop struck inside the session becomes the reason `` `cancelled `` or
  `` `timed-out `` (codes 143 and 124), which `await` and `poll` report as a
  failure. Every other stop passes as an escape.

### The pipeline's one forgiven death

**An interior edge is dead once its reader stage has ended, and a stage feels
that death only at its next write to it**
([[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]]).

- ral holds a duplicate of each interior edge's read end, so no interior edge
  ever delivers `SIGPIPE` or `EPIPE`. Once the reader is gone, ral reads the
  edge, discards what is pending, and kills the writer at the first byte
  written after: a `reader-gone` stop, which the pipeline forgives.
- `SIGPIPE` has no special case. A stage killed by a pipe of its own making is
  `signaled 13`. A producer that exits on its own keeps its status:
  `sh -c 'exit 3' | true` fails with 3.
- In the final stage the same condition is the closed-stdout stop: the run
  ends silently with 141.

### Report facts and hints

**What a renderer adds is recomputed from the record and the run, never
stored in it** ([[decisions/261105_the-error-record|the error record]]).

- **Unwind facts.** `Unwound { abandoned_tail, discarded }`: whether the
  failure abandoned written steps of a block, and how many captured bytes a
  failed `let` threw away. The abandoned-tail note is omitted for
  `not-found` and `not-runnable`, and whenever a hint is present.
- **Hints**, first match wins: a situational hint from the failing site; then
  the reason's own, signal advice for `signaled`; then the session's
  exit-hints table, keyed on the basename of `program` and the status, for
  `exited` only (`ExitHints`, `core/src/shell/runtime/exit_hints.rs`).
- **A fix both enforcers suggest is worded once**, in `authority::remedy`
  (`remove_the_exclusion`, `add_to_fs`, `add_to_exec`). A guard refusal and a
  kernel-denial hint each word their own cause, then end in that text, so a
  refusal and a denial of one exclusion give one remedy.
- **The path-as-text hint (`#'…'#`) is one rule, at the machine's
  `dispatch_exec`** (`offer_path_as_text`). It is offered only to a path head
  given no arguments, which was most likely meant as text: to a refused
  admission, and to a run that named a directory or plain file. It is appended
  after the grant's own hints, never before them.
- A handler's re-raise keeps every hint the record determines and drops the
  rest.

### Who observes the ending

| observer | success | process failure or refusal | stop |
|---|---|---|---|
| `try B H` | `B`'s value | `H` applied to the record | passes |
| `?` | first success wins | next arm | passes |
| `audit B` | `` `ok `` value, trail | `` `err `` record, trail | passes, trail dropped |
| `await` | value, bytes replayed | raised again | own `cancelled`/`timed-out` converted; others pass |
| `poll` | `ready` with `` `ok `` | `ready` with `` `err `` record | as for `await` |
| worker `done` | `returned` | `failed: …` | the cause |
| top level, batch | status 0 | rendered, the reason's code | rendered, the cause's code |
| the wire | `ok` | `err` with the record | `stopped` with the cause |

On the wire the ending is `Ending<FOValue, ErrorRecord, CancelCause>`, the
record in its strict data encoding ([[design/engine-protocol|engine-protocol]]).
exarch adds an exit tip only when the reason is a process `exited`.

## See also

[[design/cbpv|cbpv]], [[design/name-resolution|name-resolution]],
[[design/effects-handlers|effects-handlers]], [[design/grant|grant]],
[[design/endings|endings]], [[design/failure|failure]],
[[design/pipelines|pipelines]], [[design/capture|capture]],
[[internals/cancellation|cancellation]],
[[internals/exec-judgment|exec-judgment]],
[[internals/an-external-end-to-end|an-external-end-to-end]],
[[internals/confined-launch|confined-launch]],
[[map/core/io-process|io-process]],
[[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]],
[[related/call-by-push-value|call-by-push-value]],
[[related/system-c|system-c]].

Cite: RATIONALE §"System calls are algebraic effects"; `docs/SPEC.md` §2.3,
§6.2–§6.8, §7.4–§7.6, §8.5–§8.6, §8.10, §9, §11.1, §11.5, §12.3, §12.10,
§13.1, §13.3, §16.3.
