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
  value binding or base frame declares is an open name for it, whether or not
  a user handler reinterprets it
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
                  → ⟨ ↯(launch cause, prov κ) | κ ⟩             launch refused (may have run)
                  → ⟨ ⇑ stop(c) | κ ⟩                        cancelled by cause c
```

A *failure* `↯` is an error record that `try` can catch; a *stop* `⇑` is an
escape that no `try` sees ([[design/endings|endings]]). The bytes the program
writes are not the result: they go to the sinks in the context.

## Which heads denote it

**A head's spelling decides whether it is looked up or goes straight to the
operation.** The classification is in `core/src/shell/exec/command.rs`
(`Command`, `Request`) and `core/src/shell/exec/resolve.rs`
(`Resolution::of`); `docs/SPEC.md` §6.2–§6.3 is normative.

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
- **`Command::shown` is the spelling every report uses**: as written, with a `~`
  head expanded. It becomes `program` in a process failure and the first
  element of an audited `argv`.
- **On Windows** a bare search appends each `%PATHEXT%` suffix, read from the
  process environment and not from `within [env: …]`; the cwd is not
  searched; `.bat` and `.cmd` files are refused at the spawn.

**A handled command is not a performance of the operation.** A named arm
receives the rendered argument list, `List String`; the catch-all receives the
name as well. Every named arm has type `U([String] → Command)` and the
catch-all `U(String → [String] → Command)`, independent of the name and Σ
([[design/effects-handlers|effects-handlers]]). Every argv base frame likewise
has `[String] → Command` by construction, with no type rule. While the arm
runs its frame is masked, so a same-name call inside it reaches the next
frame or the OS.
The head's type and argument gate come from Σ and lexical bindings alone:
a base frame keeps its in-shell gate, and every other command name keeps the
external gate. A handler cannot widen that gate; `ls [1, 2]` is refused even
when `ls` is handled. The checker imports handled names for diagnostics, not
arm schemes. No process starts for the handled call itself, and audit records
only the externals the arm runs.

## Inputs

### The program and its arguments

- **The argument list is a list of strings, one word each.** Nothing is split,
  globbed or re-lexed, at the call or in captured output
  ([[design/words|words]]).
- **Values that are not words are refused at an external name, handled or not**:
  `()`, a list, a map or record, a block, a handle, bytes. One declaration (`RefusedArg`) is
  read by the checker on concrete types and again by `prepare`, through
  `check_argv`, on the values, so the two refusals speak one language ([[invariants/exec-argv-is-words|exec-argv-is-words]]).
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
- Both rules are `EnvVars::for_child` (`core/src/environ.rs`); preparation
  applies its `ChildEnv` with `Launch::situate`.

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

**Requested placement fixes terminal input once**, then the source is wired
as it stands (`Placement::input`, interpreted by `Source::offered`).

- `Inherit`, `Join` and `Tenant` offer terminal stdin: ral's own group, a
  stage whose owner chose its source, or a fresh group lent the foreground.
- `NewSession` withholds it: a headless session reads EOF instead of a
  startup terminal.
- Pipes and files are unchanged by either input decision.
- An envelope obeys the requested input decision even though its payload
  lands in a separate session. An eligible `Tenant` therefore keeps readable
  terminal stdin in a script too, and retains a restoration-only guard, not
  a foreground loan. No controlling terminal can stop its read.

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

- `Command::lookup` walks each candidate and judges the object it reaches, by real
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
- **Authority reaches the lease through one gate.** `TerminalAccess::reach`
  requires a session lease: `Denied` never reaches it, `Leased` requires
  terminal-bound stdout, and `ExplicitLoan` reaches it regardless of stdout.
  It yields an affine permit for one pre-spawn capture. Only
  `RequestedTerminalAccess` is public to hosts; leases, permits, preparations
  and terminal holds are opaque crate resources. Saved state and key
  attribution never leave the private terminal implementation.
- **One owner decision supplies placement** (`Shell::group_owner`, then
  process-layer `Placement::command`):

  | owner | requested placement | consequence |
  |---|---|---|
  | pipeline stage | `Join(membership)` | join its owner's group; no independent capture |
  | session with a permit | `Tenant(prepared)` | capture before spawn; lead a fresh group and lend it the foreground |
  | session without a permit, interactive | `Inherit` | ral's own group |
  | session without a permit, non-interactive | `NewSession` | headless session and group |

  `EnvelopedLaunch::spawn` steps every envelope `Step::NewLeader`, regardless
  of the request. A requested tenant then keeps captured state for restoration,
  without handing foreground to the envelope or attributing its keys.
  Direct tenants lower to `Step::NewLeader`. `PreparedTerminalLoan::lend`
  accepts only `&Leader`, a certificate minted from that fresh child's pid
  in ral's own session. `Launch::spawn_leader` gives the pipeline anchor the
  same certificate; its unreaped child must still pin the ID.
- **A pipeline selects input at its handoff boundary.** It captures before
  spawning its anchor and lends only after the anchor reports readiness.
  Success selects readable input; no permit or refusal selects withheld input,
  stored once for every stage. The refusal can restore immediately because
  only the anchor exists and never changes modes. A nested pipeline joins
  with its owner's input and no capture.
- **A refused command handoff retains restoration responsibility through
  teardown.** The owned group is killed before its leader is reaped, then the
  child is killed/reaped, saved state restored, and the jail released.
  `Unstarted::Refused` names the handoff and OS error; the command may already
  have produced effects. Every failed start owns its already-started output
  drains until caller settlement: a lone run settles before propagating the
  reason; a pipeline rolls back and joins before settling and reporting.
  Joined descendants remain the enclosing owner's responsibility, never a
  reason for a borrowed-group kill. Settlement retains no reaped group ID.
- **Restoration returns what capture found.** Unix saves foreground and modes
  independently, restoring that foreground first and then termios with
  `Drain` under SIGTTOU blocking. Windows infallibly captures any available
  standard console modes and restores them on drop; no console ownership is
  transferred. An empty snapshot is valid on either platform.

- **ral has no job control.** The reaper answers every stop with `SIGCONT`, so
  Ctrl-Z suspends nothing and no stopped job ever exists
  ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
- **Windows has no terminal handoff.** A pipeline's members share the console,
  each spawned as the root of its own console group, and are supervised as one
  Job Object.

## Modes

**Every mode performs the same operation; modes differ only in the context
they wire around it.** Each runs the same three stages of `Prepared`
(`core/src/shell/exec/command.rs`): `prepare`, a start that connects the
streams the mode supplies and spawns (`start`, or `detach` for a survivor),
and `ending`
([[internals/an-external-end-to-end|an-external-end-to-end]]).

- **Building a launch only describes it; starting it acts.** `prepare` waits
  on nothing a stop must end, so it can only be refused. Two exceptions touch
  the world: a guest jail's cgroup, which `JailCgroup::drop` removes if the
  built launch is dropped, and Linux's bwrap host probe, which waits on bwrap
  children once per process. Every step that can be stopped runs in `start`,
  through the direct or enveloped spawn, under the run's scope. On Unix the spawn reads the
  scope once before the fork and then waits for the trampoline's first word
  under it; on Windows it reads the scope before the confinement, which
  stamps ACLs, and again before `CreateProcessW`. A launch is spent by its
  spawn, which takes it by value: a second spawn would read the envelope's
  exit as the program's. So `start` is the one place a launch is stopped or
  fails as a program
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]]).
  The resources, report socket and warrant are prepared before the spawn,
  and the child's own steps are data until it: the spawn installs them as one
  hook in one order (stderr merge, stream capture, limits, confinement), after
  the descriptor handoff and before its own placement, signal reset and
  exec-readiness mark
  ([[invariants/a-process-limit-binds-before-the-child-runs|a-process-limit-binds-before-the-child-runs]]).

- **A plain command.** Dispatch admits; `Command::prepare` validates
  argv and builds the launch; `Prepared::run` connects its streams,
  titles the terminal and spawns. `Started::collect` uses the shared pipeline
  collector to wait, join pumps and classify the child's end through `ending`.
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
- **Detached.** `detach description program args` is a Unix native value
  builtin, typed `String → String → [String] → F [pid: Int, desc: String]`.
  It launches with null streams and surrenders ownership, so the process
  outlives the session. The program is resolved as `^program` resolves one,
  bypassing bindings and the whole handler stack, including base frames.
  Handlers cannot intercept `detach` itself; a value binding stubs it:
  `let detach = { |desc program args| return [pid: 0, desc: $desc] }`
  (`core/src/shell/exec/command/detach.rs`, `docs/SPEC.md` §11.5).
- **A host hook.** The REPL's external editor runs `$VISUAL`/`$EDITOR`
  through `Shell::run_external`, `^program args…` for a host holding no
  syntax, which skips bindings and handlers.
  Every other hook, prompt, plugin event or keybinding reaches exec only by
  evaluating ral code, and so through a plain command or a stage.

### Confined launch

**A restrictive grant confines each external at its own spawn.** It applies
when `SandboxProjection::of` finds something to enforce: a restricted
filesystem, an offline network, a withheld GPU, or a restricted exec table
where the kernel can render one.

- **macOS.** ral re-execs its own pinned binary as `ral --warrant <w> <r>`;
  slot `w` carries a *warrant*, the compiled Seatbelt profile and the
  program, and slot `r` the report socket. The child enters the profile and `execve`s the program in place,
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
the bytes are not the result (`docs/SPEC.md` §6.8).

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
§8.5–§8.6, §8.10). A failure carries a typed reason, a Rust-side fact
that a program never reads, and is projected to the record it does read:

```text
Reason     = `process [program: String, failure: Failure]
           | `timed-out | `cancelled | `raised [message: String, status: Int]
           | `denied Denial | `io IoFailure | `invalid InvalidFailure
           | `evaluation EvalFailure | `state StateFailure | `compile CompileFailure
           | `launch LaunchFailure | `protocol ProtocolFailure | `host HostFailure
           | `invariant InvariantFailure
Failure    = `exited Int | `signaled Int | `not-found
           | `not-runnable [found: String, why: Unrunnable]
Unrunnable = `permission-denied | `directory | `not-executable
           | `missing-interpreter (Option String) | `io IoCause
ErrorFact   = [frame: String, reason: Reason, site: [script, line, col]]
ErrorRecord = [frame: String, message: String, status: Int, site: [script, line, col]]
```

`program` is the shown head. `frame` and `site` are provenance only: the
innermost visible frame and its source position, never the subject of the
wording (`core/src/fact/error.rs`; `core/src/shell/runtime/error.rs`). The
`message` and `status` of the record are projections of the reason
(`Reason::describe`, `FailureCode`), never stored beside it.

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
| `not-runnable … permission-denied` | the spawn reports `EACCES`; or a path head's or `PATH` candidate's walk is refused; or a confined script's interpreter is hidden by the launch's own projection (`ENOENT` or `EACCES`), with the interpreter hint | failure | 126 |
| `not-runnable … io cause` | any other spawn error, or any other failure of a path head's or `PATH` candidate's walk | failure | 126 |
| grant refusal | the judgment denies the program | failure, `denied` | 1 |
| argument refusal | an argument is not a word | failure, `invalid` | 1 |
| launch refused | startup is refused: the envelope or launcher fails to exec, a step of ral's own before the `execve` fails, the trampoline refuses, the envelope dies before the program starts, or a terminal handoff is refused after exec | failure, `launch` | 1 |
| stopped | a death by a signal of a cause in force, or a cause the start sees before the program runs | stop | 124, 130, 131, 141, 143 |
| cut at the write | a non-final stage writes after its reader ended | stop, forgiven by its pipeline | none |

The clamp's floor is 1, so a negative Windows `NTSTATUS` never reads as
success.

### Found before the spawn, or by it

**Most failures are found by the probe, before any process exists**
(`Command::lookup` in `command.rs`). A *walk* is the component-by-component
resolution of a path that the judgment also reads.

- **The probe** reports absence as `not-found`; at a path head, a directory as
  `directory` and any other present non-program as `not-executable`; at a
  `PATH` candidate only a regular file lacking an execute bit counts, and
  every other present non-program is passed over as absent. Any execute bit
  counts, whoever owns it. A walk that is refused, at a path head or a `PATH`
  candidate, is `permission-denied`; one that fails in any other way but
  absence is `io`; a candidate's names the file it was reaching. Failing
  every `PATH` candidate, the first that was refused, not executable or
  whose walk failed says why, else `not-found`.
- **The spawn** reports what the program's own `execve` met, and the image
  says what it means (`explain_exec_error`). `EACCES` is
  `permission-denied`; everything else but `ENOENT` is `io`. **`ENOENT` for a
  file still at its real path is `missing-interpreter`**, naming the `#!`
  interpreter read when the launch was built, or the loader a native image
  needs; for a bundled tool or a file gone, it is `not-found`. A script with no
  `#!` line (`ENOEXEC`) is `io`; an execute bit for another user or a `noexec`
  mount is `EACCES`.
- **Only a permission refusal is `permission-denied`.** A walk that fails
  otherwise reads as itself, `io` with the OS's words, at a path head and a
  `PATH` candidate alike.
- **A confined script whose interpreter the launch's own projection hides is
  `permission-denied` with the interpreter hint**, whatever `errno` the kernel
  chose: Seatbelt refuses the exec (`EACCES`), bwrap hides the interpreter
  (`ENOENT`), whether an exclusion, the exec table or the read set withholds
  it. The hint comes from this launch's own projection, asked only once the
  start has failed, about the interpreter read with the image; with no projection (the guest jail) there is no hint and a hidden
  interpreter reads as missing.
- **A start is the program's failure only if the program's `execve` failed**,
  and a refusal in ral's own launch steps retains a typed `launch` cause
  ([[invariants/ral-s-failure-is-never-the-programs|ral-s-failure-is-never-the-programs]],
  [[invariants/no-mark-ral-s-fault|no-mark-ral-s-fault]]).
- **A grant refusal is a typed `denied` reason, not a process failure**, because the
  program was never looked at: `command 'curl' denied by active grant: …`
  (`Shell::exec_refused`, `authority/guard`). The refusal is recorded as an
  exec `check` in audit and reveals nothing about the file. A refusal precedes
  absence: a denied path that names nothing is still denied.

### Stops: cancellation and signals

**A death is a cancellation exactly when it is by a signal of a cause in
force** (`WaitOutcome::cancelled_by`, `core/src/process/outcome.rs`;
[[internals/cancellation|cancellation]];
[[decisions/260706_signals-are-causes|signals-are-causes]]).

- A cause reaches a child as a cause-directed teardown, sent to its group
  when the child leads one and to its pid otherwise; which signal opens it,
  and the grace before the kill, is
  [[internals/cancellation#External children: teardown by cause|cancellation]]'s.
- **A death by any other signal is the program's own** `signaled s`, a
  `SIGSEGV` during a teardown included. **An exit is never a signal**:
  `exit 130` is the program's choice. A confined program's end is its
  supervisor's exact word
  ([[invariants/a-supervisor-never-execs|a-supervisor-never-execs]],
  [[invariants/the-ending-is-amended-where-it-is-reaped|the-ending-is-amended-where-it-is-reaped]]).
- **Only a terminal ral lent can turn a key into a cancellation.** While a
  command or pipeline holds the terminal, the kernel delivers Ctrl-C to it,
  not to ral; ral hears the key back when it reclaims the terminal and strikes
  the run ([[decisions/260924_the-lent-terminal-returns-the-gesture|the-lent-terminal-returns-the-gesture]]).
  Without a loan (exarch, a capture without explicit access, a worker), a `SIGINT` death is
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
  failed `let` threw away. The abandoned-tail note is omitted for `not-found`
  and `not-runnable`, and whenever any hint or advice is shown.
- **Hints and advice.** A site attaches a *hint* to the error as it unwinds;
  `Reason::advice` derives intrinsic advice, including denial remedies and
  signal advice. For `exited`, the renderer adds the session's exit-hints entry,
  keyed on program basename and status. SPEC §13.1 defines composition:
  denial remedies precede stored hints; other reasons show stored hints,
  report-context advice, then intrinsic and exit-table advice. Stored hints
  retain attachment order; all notes are shown.
- **A fix both enforcers suggest is worded once**, in `authority::remedy`
  (`remove_the_exclusion`, `add_to_fs`, `add_to_exec`). A guard refusal and a
  kernel-denial hint each word their own cause, then end in that text, so a
  refusal and a denial of one exclusion give one remedy.
- **The path-as-text hint (`#'…'#`) is one rule, at the machine's
  `dispatch_exec`** (`offer_path_as_text`). It is offered only to a path head
  given no arguments, which was most likely meant as text: to a refused
  admission, and to a run that named a directory or plain file. It is appended
  after the denial's intrinsic remedy, never before it.
- A handler's re-raise keeps the record, so its advice is recomputed, and
  drops the stored hints.

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
| the wire | `ok` | `err` with the fact | `stopped` with the cause |

On the wire the ending is `Ending<FOValue, ErrorFact, CancelCause>`, the
fact, typed reason included, in its strict data encoding ([[design/engine-protocol|engine-protocol]]).
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
