---
status: proposed
---

# Windows pipeline spawn is a creation-time boundary

**Windows pipeline launch needs a custom `CreateProcessW` layer: helper protocol
handles cross by explicit handle list, and pipeline children enter their Job
Object before user code can run.** The current `std::process::Command` paths
collect and reap well, but they express two spawn facts too late or too globally.
For ral's sandbox story, process creation is an authority boundary; on Windows it
must be made as explicit as the Unix `pre_exec` boundary.

This ADR records only the invariant-restoring fix, not an operational mitigation.
It applies to the Windows arms named by
[[internals/pipeline-execution|pipeline-execution]]: helper protocol handle
passing (`core/src/runtime/pipeline/protocol/windows.rs`) and pipeline Job Object
placement (`core/src/process/signal/windows.rs`).

## Context

The Unix launcher has two pre-user-code hooks that Windows' safe `Command` API
does not give us in the same form.

- **File descriptor inheritance is per child.** Unix can clear `FD_CLOEXEC` only
  in the child-side `pre_exec` window, after `fork` and before `exec`, so an
  unrelated concurrent spawn does not see the helper's private channels.
- **Process-group placement precedes user code.** Unix `setpgid` runs in
  `pre_exec`, before the launched program can fork or exec its own descendants.

The Windows pipeline path currently approximates those properties after the
fact.

- **Helper channels are made parent-inheritable.** `pass` marks a gate, report,
  value, or helper job handle inheritable in the parent, writes its numeric value
  into the child's environment, then relies on the helper to clear inheritance
  after start. During that parent-side window, any unrelated concurrent Windows
  spawn with inheritable handles enabled can inherit the channel and keep it open.
  The symptom is not just a leak: a stray report/value/gate handle can keep EOF
  from arriving and turn collection into a wait on a process that does not own the
  protocol.
- **Pipeline Job Object membership is post-spawn.** `spawn_with_pgid` creates the
  process, then `new_leader` or `join` assigns it to the pipeline Job Object. A
  direct external stage that forks before assignment can leave descendants outside
  the job, so abort-time `TerminateJobObject` is no longer a tree kill. Helper
  stages are usually protected by their gate, but the invariant cannot depend on
  routing hostile direct externals through a helper.

Both holes belong to launch, not to collection. The collector can only wait on
and kill the process tree it was actually handed.

## Decision

Build one Windows spawn layer for pipeline children, using raw `CreateProcessW`
and an extended startup attribute list where that is the precise primitive.

- **Protocol handles cross by explicit handle list.** A helper child receives
  only the handles named for that launch: gate, report, optional value channels,
  and any helper-owned job/control handle. The parent no longer flips a global
  inherit bit on those handles before spawn. The intended Windows primitive is
  `STARTUPINFOEXW` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; the environment may
  still carry numeric handle values, but those numbers name handles that were
  deliberately admitted to this child and no other. If the Win32 ABI requires
  the listed handles to be inheritable at the instant of creation, the launcher
  owns that temporary launch record under a process-wide launch mutex:
  set-inherit, `CreateProcessW`, then clear-inherit is one critical section.
  Ral-owned Windows spawns that inherit handles must use explicit lists, never
  ambient `bInheritHandles` inheritance; the mutex is still kept as
  belt-and-braces against dependency or missed std spawns in the same process.
- **Job Object placement happens at creation time.** A direct or helper pipeline
  child is a member of the pipeline Job Object before its program can run. Prefer
  `PROC_THREAD_ATTRIBUTE_JOB_LIST` where available. Otherwise create suspended,
  assign the process to the already-known job, then resume the primary thread.
  For a new leader, create the Job Object before launch; for a joiner, look up the
  leader's job before launch.
- **`std::process::Command` remains above the boundary.** Callers may still build
  argv, cwd, env, stdio, and command image facts with the existing abstractions,
  but the final Windows launch step is owned by the custom layer because that is
  where handle admission and job membership become indivisible.
- **The sandbox and pipeline code should share the low-level Windows launch
  machinery.** The reusable core is not policy: it is UTF-16 command-line
  construction, inheritable-stdio setup, extended attribute-list lifetime
  management, suspended-process resume/close cleanup, and error rendering. Policy
  stays at the caller: helper protocols choose handles; pipeline groups choose
  jobs; sandbox projections choose capability shape.

## Consequences

- A report/value/gate channel cannot be held open by an unrelated concurrent
  spawn. EOF on the protocol again means "every admitted owner is done or
  dropped," not "some other child may have inherited a copy."
- A direct external stage cannot early-fork outside the pipeline Job Object.
  Abort, cancellation, and collection recover the tree ral meant to launch.
- Helper routing remains a semantic choice, not a safety crutch. Direct external
  stages are safe as direct externals; helper stages are chosen for evaluator,
  value-edge, redirect, audit, or terminal reasons, not because Windows launch is
  otherwise porous.
- The Windows and Unix stories line up at the invariant level: the child begins
  life with exactly the authority and group membership the parent decided.

## See also

[[internals/pipeline-execution|pipeline-execution]],
[[decisions/260617_sandbox-external-children|sandbox-external-children]],
[[decisions/260610_child-eval-unification|child-eval-unification]],
[[decisions/260610_value-edge-locality|value-edge-locality]],
[[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]],
[[map/core/runtime|runtime]], and [[map/core/io-process|io-process]].

## Full implementation plan

The clean implementation is one launch value and one interpretation point. **A
child's creation-time authority must be present in the value handed to the OS,
not reconstructed from side effects before and after spawn.** Keep the type
surface small; make the invariant large.

### 1. Dependencies and non-dependencies

Use the crates already suited to the job.

- **`windows-sys = "0.61"`** stays the Win32 FFI crate. Add features only if the
  compiler asks for them; the current `core` Windows feature set already includes
  `Win32_Foundation`, `Win32_Security`, `Win32_Storage_FileSystem`,
  `Win32_System_Console`, `Win32_System_IO`, `Win32_System_JobObjects`,
  `Win32_System_Memory`, `Win32_System_Pipes`, and `Win32_System_Threading`.
- **`os_pipe = "1"`** stays the anonymous-pipe crate for the protocol and stdio
  carriers. On Windows, convert through `std::os::windows::io::{AsRawHandle,
  FromRawHandle, OwnedHandle}` at the boundary.
- **`tempfile = "3.27"`** is already a dev dependency and is enough for Windows
  tests that need temporary `.bat`/`.cmd` files or helper executables.
- **Do not use `shlex = "2.0"` here.** `shlex` is Unix shell lexical syntax.
  Windows `CreateProcessW` receives one command-line string, parsed later by the
  target runtime or by `cmd.exe`; shlex quoting is the wrong language.
- Do not add `winapi` beside `windows-sys`, and do not add the high-level
  `windows` crate unless typed COM/HSTRING APIs become necessary. They do not
  for `CreateProcessW`, Job Objects, handle lists, or pipes.
- Do not depend on a third-party Windows quoting crate unless it is audited
  against Rust std's current tests. The safer default is a small local
  `windows_args` module derived from Rust std's
  `library/std/src/sys/args/windows.rs`, with attribution and copied test cases.

Crate search did not turn up a dependency that owns this exact boundary.
`process-wrap` is a good `Command` wrapper for process groups / Windows Job
Objects, but it still composes around `std::process::Command` and returns a
wrapped child, so it cannot express an explicit handle-list `CreateProcessW`
launch. `rappct` is useful prior art for Windows AppContainer / LPAC launch with
`STARTUPINFOEX`, but its policy is AppContainer security capabilities, not ral's
pipeline handle admission and pgid-shaped Job Object registry. `winsplit` splits
a command line into argv; ral needs the inverse operation plus batch-file
hardening. `tokio-process-tools` is async subprocess orchestration, not a raw
creation-time authority layer.

### 2. Introduce one launch value

Add `core/src/process/launch.rs` with one owned `Launch` struct. Reuse existing
types rather than minting a taxonomy:

- image and argv from `runtime::command::vet::{ExecImage, SpawnPlan}`;
- cwd and environment edits from the existing `build_command` / `apply_env`
  facts;
- stdio as ordinary `std::process::Stdio` / pipe endpoints until the backend
  lowers them;
- group intent as the existing `PgidPolicy`;
- one `Vec<RawHandle>` or small newtype for Windows-only admitted helper handles;
- one `u32` creation-flags field for Windows, carrying at least
  `CREATE_NEW_PROCESS_GROUP` when the policy requires it.

The first pass may implement `Launch::to_command()` on Unix and on Windows,
still calling `Command::spawn`. That pass should not change behaviour; it only
makes launch a value every caller can see.

### 3. Make `ChildHandle` the returned process

Stable Rust exposes no public constructor for `std::process::Child` from a raw
Windows `PROCESS_INFORMATION`, so raw `CreateProcessW` cannot honestly return
`Child`. Generalise `crate::process::ChildHandle` instead:

- Unix variant: wraps `std::process::Child` as today.
- Windows variant: owns the process handle, pid, optional `ChildStdout` and
  `ChildStderr` handles, and any thread handle that must be closed after resume.
- Public methods stay the current small set: `id`, `kill`, `take_stdout`,
  `take_stderr`, `wait_handling_stop`, `try_wait_handling_stop`, and `reap`.

Then migrate `RunningChild::assemble_with_owner`, `PipelineGroup::spawn`,
`spawn_with_pgid`, transport child storage, and the Windows group registry to
accept or return `ChildHandle`. The runtime above this line should not learn
`PROCESS_INFORMATION`.

### 4. Put helper handles in `Launch`

Change `core/src/runtime/pipeline/protocol/windows.rs::pass` so it no longer
sets `HANDLE_FLAG_INHERIT` directly. It should:

- write the numeric handle value into the helper environment exactly as today;
- push the raw handle into `Launch.admitted_handles`;
- leave ownership of the channel endpoint with `FrameGate` / `HelperProtocol`
  until spawn succeeds.

The helper-side code can keep parsing `JOB_HANDLE_ENV`, `REPORT_HANDLE_ENV`,
`VALUE_IN_HANDLE_ENV`, and `VALUE_OUT_HANDLE_ENV`. Only the parent-side proof
changes: a helper handle crosses only through the launch value.

### 5. Add the Windows launch mutex

`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` is an allow-list, but Windows still requires
listed handles to be inheritable and `bInheritHandles=TRUE` at creation. That
creates a real process-wide race against any concurrent spawn in the same
process.

Add a Windows-only `static LAUNCH_LOCK: Mutex<()>` in `process::launch::windows`
and hold it around:

1. setting `HANDLE_FLAG_INHERIT` on every stdio and admitted handle that must
   cross;
2. calling `CreateProcessW`;
3. clearing the inherit bit on those handles.

Every ral-owned Windows spawn should eventually pass through this lock. The lock
also protects against missed std spawns or dependency spawns in the same process.
It is cheap: process launch is already slow, and the critical section is small.

### 6. Build the Windows interpreter

Implement `process::launch::windows::spawn(Launch) -> io::Result<(ChildHandle,
Option<Pgid>)>` using `windows-sys`.

The backend owns:

- UTF-16 application path, command line, cwd, and environment block;
- `STARTUPINFOEXW` setup and teardown;
- `InitializeProcThreadAttributeList`, `UpdateProcThreadAttribute`, and
  `DeleteProcThreadAttributeList`;
- `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`;
- `PROC_THREAD_ATTRIBUTE_JOB_LIST` when available;
- fallback `CREATE_SUSPENDED` → `AssignProcessToJobObject` → `ResumeThread`;
- `CREATE_UNICODE_ENVIRONMENT`, `EXTENDED_STARTUPINFO_PRESENT`, and
  `CREATE_NEW_PROCESS_GROUP` preservation;
- RAII for process, thread, job, duplicated-process, and temporary attribute
  handles.

When a handle-list attribute is present, include every inherited handle in it:
stdio handles 0-2 as well as helper protocol handles. The attribute is an
allow-list; forgotten stdio means dead stdio.

### 7. Treat command-line quoting as security

The raw Windows launcher must not invent quoting.

Implement a local `windows_args` module by porting Rust std's Windows argument
construction:

- ordinary argv quoting equivalent to std's `append_arg` /
  `make_command_line`;
- batch-file command construction equivalent to std's `make_bat_command_line`,
  including the hardening associated with CVE-2024-24576;
- tests copied from Rust std's Windows process/args tests, plus ral cases for
  spaces, trailing backslashes, embedded quotes, empty args, and non-ASCII.

If that port is not done in the first implementation parcel, the raw Windows
path must refuse `.bat` and `.cmd` images with a clear error rather than launch
them unsafely. `shlex` is not an acceptable fallback.

### 8. Prepare and register Job Objects

Split `core/src/process/signal/windows.rs` group management into two phases.

- **Prepare before launch.** For `NewLeader` / `NewSession`, create the Job
  Object, install kill-on-close, associate the completion port, and pass the job
  to `Launch`. For `Join`, look up or duplicate the leader's job handle and pass
  it to `Launch`.
- **Register after launch.** Once `CreateProcessW` succeeds and the child is
  already in the job, register pid, duplicated process handle, leader handle, and
  completion-port state in `GROUPS`.

Delete the warning path that assigns a running child and continues on failure.
If a child cannot enter the requested Job Object before start, launch fails. This
is intentional even under CI/container hosts that place ral inside a restrictive
Job Object. The diagnostic should ask:

> could not place pipeline child in its Job Object before start; is ral already
> running in a restrictive Windows Job Object?

### 9. Migrate call sites through the choke point

Move in small compiling parcels.

1. Convert standalone external launch in `core/src/runtime/command/process.rs`
   to build and spawn `Launch`, still using `Command::spawn` internally.
2. Convert `core/src/runtime/pipeline/group.rs` so pipeline stages also spawn
   through `Launch`.
3. Generalise `ChildHandle` and update `RunningChild`, transport, and Windows
   group registry to stop exposing `std::process::Child`.
4. Switch the Windows backend to raw `CreateProcessW`.
5. Change Windows protocol `pass` to push admitted handles into `Launch`.
6. Remove post-spawn `AssignProcessToJobObject` from ordinary pipeline launch.

At every step, preserve the public rule of `spawn_with_pgid`: if it returns, the
child already has the requested group membership and admitted handles.

### 10. Test the laws

Add unit tests for:

- Windows argv and `.bat`/`.cmd` quoting;
- environment-block sorting and NUL rejection;
- handle-list construction including stdio;
- launch-lock cleanup after spawn failure;
- suspended-create cleanup when job assignment fails.

Add Windows integration tests for:

- helper report/value/gate EOF under concurrent ral-owned spawns;
- a direct external that forks immediately still dies when the pipeline Job
  Object is terminated;
- inherited stdin/stdout/stderr still work when a handle list is present;
- restrictive outer Job Object failure produces the deliberate diagnostic;
- Ctrl-Break fan-out still works, proving `CREATE_NEW_PROCESS_GROUP` survived.

Non-Windows CI should still exercise `Launch` by rendering it into the existing
Unix `Command` path.

### 11. Remove obsolete text and restamp the wiki

Once the code lands, delete comments that describe the handle-inheritance race or
post-spawn job escape as live behaviour. Update
[[internals/pipeline-execution|pipeline-execution]] and restamp the affected
[[map/core/runtime|runtime]] / [[map/core/io-process|io-process]] map pages in
the same commit.

Failure must be loud and specific throughout. If Windows cannot create the
requested handle list, say "could not launch pipeline helper with an explicit
handle list". If it cannot place a child in its Job Object before start, say
"could not place pipeline child in its Job Object before start". Those are not
performance degradations; they are sandbox-boundary failures.
