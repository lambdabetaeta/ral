# Structured wait outcomes and terminal policy

## Purpose

This plan fixes the `cat README.md | glow -p` class of bug at the
right layer.

There are two separate problems:

1. ral collapses process outcomes into `i32` too early. A command that
   exits with code 137, a command killed by SIGKILL, and a command
   stopped by terminal job control and then killed by ral can all become
   the same number. Diagnostics then reconstruct meaning from that
   number and can lie.
2. Pipeline foreground policy is implicit and late. The pipeline planner
   decides only "pure external" versus "mixed"; terminal ownership is
   then re-derived while launching. A mixed pipeline with an interactive
   external tail can reach the kernel, get stopped by SIGTTOU or SIGTTIN,
   and only then does ral discover that the terminal policy was wrong
   for that command.

The fix is to make both concepts explicit:

- process wait results stay structured until the final OS-exit boundary;
- pipeline analysis emits a terminal plan before launch;
- terminal-sensitive tails are either foregrounded deliberately or
  rejected with a useful error before they are allowed to stop.

This document is written as an implementation plan for a less-contextual
agent. Follow the phases in order. Do not skip the tests at the end of
each structural phase.

## Current failure path

The current path for the misleading `137` diagnostic is:

1. `core/src/signal/unix.rs::wait_handling_stop` calls
   `waitpid(..., WUNTRACED)`.
2. If the child is stopped, ral sends `SIGKILL` to the pipeline process
   group.
3. ral loops, reaps the now-signalled child, and returns a plain
   `std::process::ExitStatus`.
4. `core/src/evaluator/exec/command.rs::exit_code` turns signal death
   into `128 + signal`.
5. `core/src/evaluator/exec/command.rs::external_exit_error` builds
   `"{cmd}: exited with status {code}"`.
6. `core/src/exit_hints.rs::lookup` sees `status > 128` and adds a
   synthetic "killed by signal N" hint.

The important information was lost at step 3:

- Was this a normal exit?
- Was this a real signal death?
- Was this a terminal-control stop?
- Did ral itself send the final SIGKILL?

The current terminal path is:

1. `pipeline::analysis` computes `PipelineMode::{PureExternal, Mixed}`.
2. `PipelineGroup::claim_foreground` re-derives whether to foreground
   from `(mode, shell.repl.plugin_context.in_tui)`.
3. Mixed pipelines outside `_ed-tui` do not foreground.
4. An external tail such as `glow -p`, `fzf`, `less`, `vim`, or anything
   calling `tcsetattr`/`tcgetattr` may try to use the controlling tty.
5. The kernel stops it with SIGTTOU or SIGTTIN because its process group
   is not foreground.
6. ral kills the group and reports a SIGKILL-shaped status.

The first fix makes that report honest. The second fix keeps the known
terminal-tail cases from reaching that trap accidentally.

## Desired behavior

### Process outcome behavior

Normal non-zero exit:

```text
grep: exited with status 1
hint: no matches found
```

Real signal death:

```text
sh: killed by signal 9 (SIGKILL)
```

Terminal job-control stop induced by a mixed pipeline:

```text
glow: stopped by signal 22 (SIGTTOU) while accessing the terminal
hint: ral killed the pipeline because glow tried to use the terminal from a background process group. The pipeline was mixed, so ral did not hand the terminal to the external process group. Is an earlier stage internal, an alias, a handler, or a builtin?
```

User stop in a pipeline:

```text
cmd: stopped by signal 20 (SIGTSTP); ral killed the pipeline
hint: ral has no job control yet. Stopping a command inside a pipeline tears the pipeline down.
```

SIGPIPE in a non-final pipeline stage:

```text
yes | head -n 1
```

must still succeed. This must be represented as
`WaitOutcome::Signaled(SIGPIPE)` forgiven for a non-final stage, not
as magic status 141.

### Terminal policy behavior

A pipeline plan must explicitly say what terminal ownership will happen.

There are four basic cases:

1. No external stages. No foreground handoff.
2. All external stages. Foreground the pipeline process group in an
   interactive tty.
3. Mixed pipeline inside `_ed-tui`. Foreground the external group because
   the editor yielded the terminal by contract.
4. Mixed pipeline outside `_ed-tui`.

Case 4 is the key. It must split further:

- If no terminal-sensitive external stage is present, preserve current
  behavior. Do not foreground.
- If a terminal-sensitive external stage is present and the pipeline can
  be safely foregrounded under a deliberate rule, foreground it.
- If it cannot be safely foregrounded, fail before launch with a
  diagnostic that says the external stage needs the terminal but the
  pipeline contains ral-internal stages.

The practical first implementation should reject known terminal tails in
ordinary mixed pipelines. It is safer than trying to infer that internal
stages will never read the tty. A later implementation can add an
explicit user syntax or metadata to allow a deliberate foregrounded
mixed pipeline.

## High-level design

Introduce these concepts:

```rust
pub enum WaitOutcome {
    Exited(i32),
    Signaled(Signal),
    StoppedThenKilled {
        stopped_by: Signal,
        killed_by: Signal,
    },
    NativeCode(i32),
}
```

```rust
pub struct Signal {
    pub number: i32,
}
```

```rust
pub enum CommandFailure {
    ExitCode(i32),
    Signal(Signal),
    StoppedByJobControl {
        stop_signal: Signal,
        killed_by: Signal,
    },
    Spawn(SpawnFailure),
}
```

```rust
pub enum Status {
    Code(i32),
    Process(CommandFailure),
}
```

```rust
pub enum TerminalPlan {
    NoTerminal,
    ForegroundExternalGroup,
    ForegroundUnderEdTui,
    MixedNoForeground,
    RejectTerminalSensitiveMixed {
        stage_name: String,
        reason: TerminalSensitivityReason,
    },
}
```

Keep these principles:

- `WaitOutcome` is what the OS told us.
- `CommandFailure` is what the user should understand.
- `Status` is how ral carries failure through existing `Error` values.
- `Error::exit_code()` is the only ordinary way to reduce an error to an
  integer for process exit or `$status`.
- `ExitHints` is only a table lookup. It must not synthesize signal
  meaning from `status > 128`.
- Pipeline terminal ownership is decided once in `resolve_pipeline`.
  Launch consumes that decision.

## Phase 1: add signal and wait outcome types

### Files

- Add `core/src/signal/outcome.rs`.
- Update `core/src/signal.rs` to `mod outcome;` and re-export the public
  pieces.

### Types

Use a small `Signal` newtype instead of raw `i32` everywhere. It keeps
signame formatting in one place and avoids repeating array lookups.

```rust
//! Structured process wait outcomes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    number: i32,
}

impl Signal {
    pub fn new(number: i32) -> Self;
    pub fn number(self) -> i32;
    pub fn name(self) -> Option<&'static str>;
    pub fn display(self) -> String;
    pub fn is_sigpipe(self) -> bool;
    pub fn is_job_control_stop(self) -> bool;
    pub fn user_exit_code(self) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Exited(i32),
    Signaled(Signal),
    StoppedThenKilled {
        stopped_by: Signal,
        killed_by: Signal,
    },
    NativeCode(i32),
}

impl WaitOutcome {
    pub fn from_exit_status(status: std::process::ExitStatus) -> Self;
    pub fn to_user_exit_code(self) -> i32;
    pub fn is_success(self) -> bool;
    pub fn is_normal_exit(self) -> bool;
    pub fn is_broken_pipe(self) -> bool;
}
```

### Unix details

In `Signal::name`, support at least signals 1 through 31. Put the table
in `outcome.rs`. If a signal is outside the table, format as
`signal N`.

`Signal::display()` should return:

- `9 (SIGKILL)` when the name is known;
- `64` when the name is unknown.

`Signal::is_job_control_stop()` should return true for:

- SIGTSTP;
- SIGTTIN;
- SIGTTOU;
- SIGSTOP.

`WaitOutcome::from_exit_status` on Unix:

```rust
if let Some(code) = status.code() {
    WaitOutcome::Exited(code)
} else if let Some(sig) = status.signal() {
    WaitOutcome::Signaled(Signal::new(sig))
} else {
    WaitOutcome::NativeCode(1)
}
```

`WaitOutcome::to_user_exit_code`:

- `Exited(code)` -> `code`;
- `Signaled(sig)` -> `128 + sig.number()`;
- `StoppedThenKilled { stopped_by, .. }` -> `128 + stopped_by.number()`;
- `NativeCode(code)` -> `code`.

The choice for `StoppedThenKilled` is deliberate. The user-facing cause
is the stop. The cleanup SIGKILL is secondary.

`WaitOutcome::is_broken_pipe`:

- true for `Signaled(SIGPIPE)`;
- true for `NativeCode(code)` when `(code as u32) == 0xC000_00B1`;
- false otherwise.

### Windows details

In `core/src/signal/windows.rs`, `wait_handling_stop` can continue to
call `child.wait()?`, then return `WaitOutcome::from_exit_status(status)`.

Do not invent Windows signal names. `NativeCode` is enough unless the
platform already exposes a signal.

### Fallback details

In the non-Unix/non-Windows fallback in `core/src/signal.rs`, return
`WaitOutcome::from_exit_status(child.wait()?)`.

### Compile target for phase 1

At the end of phase 1, the new module should compile but may not yet be
used by `RunningChild`. Run:

```sh
docker exec shell-dev cargo build 2>&1 | tail -20
```

## Phase 2: return WaitOutcome from wait_handling_stop

### Files

- `core/src/signal/unix.rs`
- `core/src/signal/windows.rs`
- `core/src/signal.rs`
- `core/src/evaluator/exec/child.rs`
- any direct callers of `wait_handling_stop`

### Unix algorithm

Change the signature:

```rust
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
) -> std::io::Result<WaitOutcome>
```

Implement this logic:

1. Call `waitpid(pid, &mut status, WUNTRACED)`.
2. If interrupted by EINTR, loop.
3. If `WIFSTOPPED(status)`:
   - store `stopped_by = Signal::new(WSTOPSIG(status))`;
   - send SIGKILL to `-pgid` or `child.kill()` if there is no pgid;
   - loop until the same pid is reaped;
   - when the reaped status is signalled, return
     `StoppedThenKilled { stopped_by, killed_by }`;
   - when the reaped status exits normally, return
     `StoppedThenKilled { stopped_by, killed_by: Signal::new(SIGKILL) }`
     or `Exited(code)` only if a normal exit after SIGKILL is genuinely
     possible and desired. Prefer the first option; it preserves ral's
     intent.
4. If `WIFEXITED(status)`, return `Exited(WEXITSTATUS(status))`.
5. If `WIFSIGNALED(status)`, return `Signaled(Signal::new(WTERMSIG(status)))`.
6. Otherwise return `NativeCode(status)`.

Keep the existing debug trace, but update it to include the signal name.

### RunningChild changes

In `core/src/evaluator/exec/child.rs`:

```rust
pub(crate) struct WaitedChild {
    pub outcome: WaitOutcome,
    pump: Option<std::thread::JoinHandle<()>>,
    stderr_pump: Option<std::thread::JoinHandle<()>>,
}
```

`RunningChild::wait` now stores `outcome`.

### Important invariant

No code should call `std::process::ExitStatus::code()` after this phase
except inside `WaitOutcome::from_exit_status`.

Search:

```sh
rg -n "ExitStatus|\\.code\\(\\)|\\.signal\\(\\)|exit_code\\(" core ral exarch
```

Some test code may still call it. Runtime code should not.

### Compile target for phase 2

The build will fail until exec and pipeline consumers are migrated. Do
not leave the branch in this state. Continue immediately to phase 3.

## Phase 3: introduce CommandFailure

### Files

- Add `core/src/evaluator/exec/failure.rs`.
- Update `core/src/evaluator/exec.rs` to `mod failure;` and export the
  types needed by pipeline code.

### Types

```rust
//! User-facing external-command failures.

use crate::signal::{Signal, WaitOutcome};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnFailure {
    NotFound,
    PermissionDenied,
    Io(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandFailure {
    ExitCode(i32),
    Signal(Signal),
    StoppedByJobControl {
        stop_signal: Signal,
        killed_by: Signal,
    },
    Spawn(SpawnFailure),
}
```

### Conversion from WaitOutcome

```rust
impl CommandFailure {
    pub fn from_outcome(
        outcome: WaitOutcome,
        is_pipeline_non_final: bool,
    ) -> Option<Self> {
        match outcome {
            WaitOutcome::Exited(0) => None,
            WaitOutcome::Exited(code) => Some(Self::ExitCode(code)),
            WaitOutcome::Signaled(sig)
                if is_pipeline_non_final && sig.is_sigpipe() => None,
            WaitOutcome::Signaled(sig) => Some(Self::Signal(sig)),
            WaitOutcome::StoppedThenKilled { stopped_by, killed_by } => {
                Some(Self::StoppedByJobControl {
                    stop_signal: stopped_by,
                    killed_by,
                })
            }
            WaitOutcome::NativeCode(code)
                if is_pipeline_non_final
                    && WaitOutcome::NativeCode(code).is_broken_pipe() => None,
            WaitOutcome::NativeCode(0) => None,
            WaitOutcome::NativeCode(code) => Some(Self::ExitCode(code)),
        }
    }
}
```

### Message formatting

Add methods:

```rust
impl CommandFailure {
    pub fn message(&self, cmd: &str) -> String;
    pub fn default_hint(&self, cmd: &str) -> Option<String>;
    pub fn to_user_exit_code(&self) -> i32;
}
```

Messages:

- `ExitCode(n)`:
  `"{cmd}: exited with status {n}"`
- `Signal(sig)`:
  `"{cmd}: killed by signal {sig.display()}"`
- `StoppedByJobControl { stop_signal, .. }` with SIGTTIN:
  `"{cmd}: stopped by signal {stop_signal.display()} while reading from the terminal"`
- `StoppedByJobControl { stop_signal, .. }` with SIGTTOU:
  `"{cmd}: stopped by signal {stop_signal.display()} while configuring the terminal"`
- `StoppedByJobControl { stop_signal, .. }` with SIGTSTP or SIGSTOP:
  `"{cmd}: stopped by signal {stop_signal.display()}; ral killed the pipeline"`
- other stopped signals:
  `"{cmd}: stopped by signal {stop_signal.display()}; ral killed the pipeline"`
- `Spawn(NotFound)`:
  use the existing `compat::not_found_hint(cmd)` as the message.
- `Spawn(PermissionDenied)`:
  `"{cmd}: permission denied"`
- `Spawn(Io(msg))`:
  `"{cmd}: {msg}"`

Hints:

- `ExitCode(_)`: no default hint. File-backed `ExitHints` handles these.
- `Signal(SIGKILL)`:
  `"the process was killed with SIGKILL; the kernel or another process may have terminated it"`
- `Signal(SIGSEGV)`:
  `"the process crashed with a segmentation fault"`
- generic `Signal(sig)`:
  `"the process terminated from {sig.display()}"`
- `StoppedByJobControl(SIGTTIN)`:
  `"ral killed the pipeline because {cmd} tried to read the terminal from a background process group. Is an earlier stage internal, an alias, a handler, or a builtin? Use `help {cmd}` to inspect command resolution."`
- `StoppedByJobControl(SIGTTOU)`:
  `"ral killed the pipeline because {cmd} tried to configure the terminal from a background process group. This often means an interactive tail ran in a mixed pipeline."`
- `StoppedByJobControl(SIGTSTP | SIGSTOP)`:
  `"ral has no job control yet. Stopping a command inside a pipeline tears the pipeline down."`
- `Spawn(NotFound)`: no additional hint if the message already includes
  the compatibility hint. Avoid duplicating text.

Exit code mapping:

- `ExitCode(n)` -> `n`;
- `Signal(sig)` -> `128 + sig.number()`;
- `StoppedByJobControl { stop_signal, .. }` -> `128 + stop_signal.number()`;
- `Spawn(NotFound)` -> `127`;
- `Spawn(PermissionDenied)` -> `126`;
- `Spawn(Io(_))` -> `127`.

### Spawn error migration

Current `spawn_error` returns `EvalSignal` directly. Keep a wrapper for
compatibility, but build from `CommandFailure::Spawn`.

Later phases can simplify.

## Phase 4: make Error.status structured

### Files

- `core/src/types/error.rs`
- all files reading `err.status`

### Type change

Change:

```rust
pub status: i32
```

to:

```rust
pub status: Status
```

Define:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Code(i32),
    Process(crate::evaluator::exec::CommandFailure),
}
```

If this introduces an awkward dependency from `types` to `evaluator`,
move `CommandFailure` to a lower-level module, for example
`core/src/types/process.rs` or `core/src/process.rs`. Prefer a lower
module over creating a dependency cycle. The final architecture should
not make `types` depend on `evaluator`.

Recommended dependency-safe layout:

- `core/src/process.rs`: `Signal`, `WaitOutcome`, `CommandFailure`,
  `SpawnFailure`.
- `core/src/signal/outcome.rs` may not be needed if `process.rs` owns
  the types.

If using `core/src/process.rs`, still re-export `WaitOutcome` from
`signal.rs` for call-site ergonomics.

### Constructors

Keep `Error::new(msg, status: i32)` unchanged at the call site:

```rust
pub fn new(msg: impl Into<String>, status: i32) -> Self {
    Self {
        message: msg.into(),
        status: Status::Code(status),
        ...
    }
}
```

Add:

```rust
pub fn from_command_failure(
    cmd: &str,
    failure: CommandFailure,
    loc: crate::diagnostic::SourceLoc,
    shell: &crate::types::Shell,
) -> Self
```

This should:

1. Build `message = failure.message(cmd)`.
2. Set `status = Status::Process(failure.clone())`.
3. Set `loc`.
4. Set `hint` using this precedence:
   - explicit caller hint if one is later attached;
   - `failure.default_hint(cmd)`;
   - for `CommandFailure::ExitCode(code)`, `shell.exit_hints.lookup(cmd, code)`.

Add:

```rust
pub fn exit_code(&self) -> i32 {
    match &self.status {
        Status::Code(code) => *code,
        Status::Process(failure) => failure.to_user_exit_code(),
    }
}

pub fn status_code_for_display(&self) -> Option<i32> {
    match &self.status {
        Status::Code(0) => None,
        Status::Code(code) => Some(*code),
        Status::Process(_) => None,
    }
}
```

The display helper avoids appending `(exit status 137)` after a message
that already says "killed by signal 9".

### Migrate readers

Search:

```sh
rg -n "err\\.status|\\.status\\.clamp|status: i32|Error \\{[^}]*status" core ral exarch
```

Expected changes:

- `ral/src/main.rs`: use `e.exit_code().clamp(0, 255)`.
- `ral/src/repl/exec.rs`: use `e.exit_code()`.
- `core/src/evaluator/pipeline/collect.rs`: use `err.exit_code()`.
- any tests comparing `err.status`: update to `err.exit_code()` unless
  they are explicitly testing `Status`.

Do not change the many `Error::new(..., 1)` call sites.

### Diagnostic renderer

In `core/src/diagnostic.rs::format_runtime_error_compact`, replace:

```rust
if err.status != 0 {
    out.push_str(&format!(" (exit status {})", err.status));
}
```

with:

```rust
if let Some(code) = err.status_code_for_display() {
    out.push_str(&format!(" (exit status {code})"));
}
```

Structured process failures should not get a second parenthesized exit
status by default. Their message is already the status.

The ariadne path already uses `err.message` and `err.hint`, so it should
need little change.

## Phase 5: remove lossy exit_code and external_exit_error

### Files

- `core/src/evaluator/exec/command.rs`
- `core/src/evaluator/exec.rs`
- `core/src/evaluator/pipeline/launch.rs`
- `core/src/evaluator/pipeline/collect.rs`
- `core/src/evaluator/pipeline.rs`

### Remove

Delete:

- `exec::command::exit_code`;
- `exec::command::external_exit_error`;
- `pipeline::is_broken_pipe_exit`.

No runtime code should convert `ExitStatus` to `i32`.

### Standalone exec

In `core/src/evaluator/exec.rs`:

Current shape:

```rust
let waited = running.wait()?;
let status = waited.status;
if status.code().is_some() { commit atomic redirect }
waited.drain();
let code = exit_code(status);
shell.control.last_status = code;
if code == 0 { Ok(Unit) } else { Err(external_exit_error(...)) }
```

New shape:

```rust
let waited = running.wait()?;
let outcome = waited.outcome;

if outcome.is_normal_exit()
    && let Some(commit) = atomic_commit.take()
{
    commit.commit().map_err(...)?;
}

waited.drain();

let code = outcome.to_user_exit_code();
shell.control.last_status = code;

match CommandFailure::from_outcome(outcome, false) {
    None => Ok(Value::Unit),
    Some(failure) => Err(EvalSignal::Error(Error::from_command_failure(
        &cmd_name,
        failure,
        loc,
        shell,
    ))),
}
```

### Pipeline process join

Change `ProcessHandle::join` return type:

```rust
pub(super) fn join(
    self,
    shell: &mut Shell,
    is_last: bool,
) -> Result<(Option<CommandFailure>, i32), EvalSignal>
```

Algorithm:

1. `let waited = running.wait()?;`
2. `let outcome = waited.outcome;`
3. `let exit_code = outcome.to_user_exit_code();`
4. `let failure = CommandFailure::from_outcome(outcome, !is_last);`
5. `waited.drain();`
6. audit code records `exit_code`;
7. return `(failure, exit_code)`.

Do not call `failure.to_user_exit_code()` for audit if the stage was a
forgiven SIGPIPE. Audit should record the observed reduced code if that
is the current behavior, or 0 if audit is supposed to show effective
pipeline status. The existing code records `effective`; preserve that
meaning:

- for forgiven non-final SIGPIPE, audit status should be 0;
- for real failures, audit status should be the reduced exit code.

So compute:

```rust
let audit_status = if failure.is_none() { 0 } else { exit_code };
```

unless the existing audit specification says otherwise.

### Pipeline collect

In `observe_process`:

```rust
let (failure, code) = handle.join(shell, is_pipeline_final)?;
if let Some(failure) = failure {
    let err = Error::from_command_failure(&name, failure, loc, shell);
    self.note_failure(err.exit_code(), Some(err));
}
if is_pipeline_final {
    shell.control.last_status = code;
}
```

Be careful: if the final stage succeeds with code 0, last status is 0.
If a non-final stage fails, collector failure status becomes that
failure's `exit_code`.

### Pipeline fallback

`PipelineCollector::finish` has a fallback:

```rust
Error::new(format!("pipeline exited with status {}", self.status), self.status)
```

This can remain a synthetic `Status::Code`, because it is only used when
no structured command failure is available.

## Phase 6: make ExitHints table-only

### Files

- `core/src/exit_hints.rs`
- `data/exit-hints.txt`

Remove the synthetic signal decode:

```rust
if status > 128 { ... killed by signal ... }
```

`ExitHints::lookup` should only:

1. normalize command basename;
2. check `(name, status)`;
3. check `("*", status)`;
4. return `None`.

Update comments in `data/exit-hints.txt`:

Current comment says signal-killed exits are handled programmatically.
Keep that idea but make it precise:

```text
# Signal deaths and job-control stops are represented structurally in
# CommandFailure; do not add 128+N signal hints here.
```

## Phase 7: introduce TerminalPlan

### Files

- `core/src/evaluator/pipeline/analysis.rs`
- `core/src/evaluator/pipeline/group.rs`
- `core/src/evaluator/pipeline/launch.rs`
- maybe `core/src/evaluator/pipeline.rs`

### Types

Put this in `analysis.rs` or a new `terminal.rs` under
`core/src/evaluator/pipeline/`.

Prefer a new file if the type grows:

```rust
//! Pipeline terminal ownership planning.
```

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TerminalSensitivityReason {
    KnownInteractiveProgram,
    ExplicitTerminalMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TerminalPlan {
    NoTerminal,
    ForegroundExternalGroup,
    ForegroundUnderEdTui,
    MixedNoForeground,
    RejectTerminalSensitiveMixed {
        stage_name: String,
        reason: TerminalSensitivityReason,
    },
}
```

Add helpers:

```rust
impl TerminalPlan {
    pub fn owns_tty(&self) -> bool {
        matches!(
            self,
            TerminalPlan::ForegroundExternalGroup
                | TerminalPlan::ForegroundUnderEdTui
        )
    }

    pub fn should_foreground(&self) -> bool {
        self.owns_tty()
    }
}
```

### Terminal-sensitive command classifier

Add a conservative known-list function:

```rust
fn is_known_terminal_program(name: &str, args: &[Value]) -> bool
```

Initial known terminal programs:

- `glow` when `-p` or `--pager` is present;
- `less`;
- `more`;
- `man`;
- `vim`;
- `nvim`;
- `vi`;
- `nano`;
- `emacs`;
- `fzf`;
- `top`;
- `htop`;
- `btop`;
- `ssh` only when no command argument is present is tricky; skip it in
  the first implementation unless tests cover it.

Keep the list small and obvious. False positives cause a pre-launch
error in mixed pipelines, so do not include ordinary filters such as
`grep`, `sed`, `awk`, `sort`, `head`, `tail`, or `cat`.

Normalize command names through basename:

```rust
let bare = std::path::Path::new(name)
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or(name);
```

Argument checking should inspect stringified arguments after eval in
`StageDispatch::External`. If the values have not yet been stringified,
only accept scalar values and call `to_string()` the same way external
argv validation does. Do not re-evaluate arguments.

### PipelinePlan field

Add:

```rust
pub(super) terminal: TerminalPlan,
```

to `PipelinePlan`.

### Planning algorithm

After all stages have `StageSpec`, compute:

```rust
fn plan_terminal(specs: &[StageSpec], mode: PipelineMode, shell: &Shell) -> TerminalPlan
```

Inputs:

- `mode`;
- `shell.repl.plugin_context.as_ref().is_some_and(|pc| pc.in_tui)`;
- whether there are external stages;
- whether any external stage is terminal-sensitive.

Rules:

1. If there are no external stages:
   `NoTerminal`.
2. If `mode == PureExternal`:
   `ForegroundExternalGroup`.
3. If `mode == Mixed` and `in_tui`:
   `ForegroundUnderEdTui`.
4. If `mode == Mixed` and any external stage is terminal-sensitive:
   `RejectTerminalSensitiveMixed { stage_name, reason }`.
5. Otherwise:
   `MixedNoForeground`.

This is intentionally stricter than today's behavior. It prevents the
known bug from reaching the kernel. The structured wait path remains
needed for unknown terminal programs and real stops.

### Diagnostic for rejected terminal-sensitive mixed pipeline

Add an `EvalSignal::Error` before launch if `TerminalPlan` is
`RejectTerminalSensitiveMixed`.

Message:

```text
glow: terminal program in a mixed pipeline
```

Hint:

```text
glow appears to need the terminal, but an earlier stage runs inside ral, so ral cannot safely hand the terminal to glow. Use an all-external producer, run glow separately, or avoid pager mode.
```

Location should point at the terminal-sensitive external stage.

How to implement:

- `TerminalPlan::Reject...` needs `line`, `col`, and `len`, or the
  caller needs to recover it from the matching `StageSpec`.
- Prefer:

```rust
RejectTerminalSensitiveMixed {
    stage_name: String,
    line: usize,
    col: usize,
    reason: TerminalSensitivityReason,
}
```

Then `run_pipeline` can fail immediately after `resolve_pipeline`:

```rust
if let Some(err) = plan.terminal.rejection_error(shell) {
    return Err(EvalSignal::Error(err));
}
```

This keeps launch free of policy errors.

### Update PipelineGroup

Change:

```rust
PipelineGroup::new(plan.mode)
```

to:

```rust
PipelineGroup::new(plan.terminal.clone())
```

`PipelineGroup` should store `terminal: TerminalPlan`, not `mode`.

`claim_foreground`:

```rust
if self.terminal.should_foreground()
    && self.foreground.is_none()
    && let Some(Pgid(leader)) = self.leader
{
    self.foreground = ForegroundGuard::try_acquire(leader, shell);
}
```

Do not inspect `shell.repl.plugin_context` here. That decision belongs
to analysis.

### Update route_stdin

Current code uses `group.mode()`:

- pure external with no incoming pipe inherits stdin;
- mixed with no incoming pipe uses null.

Replace with terminal ownership:

- if incoming pipe exists: use pipe;
- else if startup stdin is not tty: inherit;
- else if `group.owns_tty()`: inherit;
- else: null.

Add a method on `PipelineGroup`:

```rust
pub(super) fn owns_tty(&self) -> bool {
    self.terminal.owns_tty()
}
```

This makes stdin routing match foreground policy.

## Phase 8: optional explicit escape hatch

Do not implement this unless the user asks, but leave the design clear.

The strict known-terminal rejection can be inconvenient. A future syntax
could allow:

```ral
within [terminal: foreground] {
    to-lines $entries | fzf
}
```

or:

```ral
to-lines $entries | ^terminal fzf
```

This would mean:

- the user asserts internal stages will not read `/dev/tty`;
- ral foregrounds the external group;
- internal stages keep stdin as pipe/null according to their pipeline
  edges;
- diagnostics mention the explicit terminal grant if a stop still occurs.

Do not add this syntax in this fix. It changes the language and requires
SPEC updates beyond diagnostics.

## Phase 9: update jobs foreground waiter

### Files

- `ral/src/jobs.rs`

There is another wait/status path in `wait_foreground` that currently
returns `(i32, was_stopped)` and re-derives `128 + signal`.

Change it to return:

```rust
(WaitOutcome, bool)
```

or a small local struct:

```rust
struct ForegroundWait {
    outcome: WaitOutcome,
    was_stopped: bool,
}
```

Only reduce to integer at the call site using `outcome.to_user_exit_code()`.

This prevents the old integer idiom from surviving in job-control code.

## Phase 10: tests

All tests that compile or run ral must run inside `shell-dev`.

Use:

```sh
docker exec shell-dev cargo test 2>&1 | tail -40
```

Add focused tests before broad tests. Do not rely on `glow` being
installed for automated tests.

### Unit tests for WaitOutcome

Location:

- `core/src/signal/outcome.rs` module tests, or
- `core/tests/...` if integration is easier.

Tests:

1. `Signal::new(9).display()` is `"9 (SIGKILL)"` on Unix.
2. `Signal::new(999).display()` is `"999"`.
3. `WaitOutcome::Signaled(SIGPIPE).is_broken_pipe()` is true on Unix.
4. `WaitOutcome::StoppedThenKilled { stopped_by: SIGTTOU, killed_by: SIGKILL }.to_user_exit_code()` is `150` because `128 + 22`.

### Unit tests for CommandFailure

Tests:

1. `Exited(0)` maps to `None`.
2. `Exited(7)` maps to `ExitCode(7)`.
3. `Signaled(SIGPIPE)` maps to `None` for non-final stages.
4. `Signaled(SIGPIPE)` maps to `Signal(SIGPIPE)` for final stages.
5. `StoppedThenKilled(SIGTTOU, SIGKILL)` maps to
   `StoppedByJobControl`.
6. `StoppedByJobControl(SIGTTOU).message("glow")` contains
   `"stopped by signal 22 (SIGTTOU)"`.
7. `StoppedByJobControl(SIGTTOU).message("glow")` does not contain
   `"137"`.

### Integration test: stopped pipeline tail

Location:

- `ral/tests/pipeline.rs`

Test command:

```ral
cat README.md | sh -c "kill -STOP \$\$"
```

Expected:

- non-zero exit;
- stderr contains `"stopped by signal"`;
- stderr contains `"SIGSTOP"`;
- stderr does not contain `"exited with status 137"`;
- stderr does not present SIGKILL as the primary cause.

Do not assert complete absence of `"SIGKILL"` if the final chosen hint
mentions that ral killed the pipeline. The invariant is that SIGKILL is
secondary, not the main message.

### Integration test: SIGTTOU-style terminal stop

Automating SIGTTOU in a non-interactive test is harder. Use a controlled
stop as above for normal CI. If a PTY test harness exists, add:

```ral
cat README.md | sh -c "kill -TTOU \$\$"
```

Expected:

- stderr says `"stopped by signal 22 (SIGTTOU)"`;
- hint mentions terminal/background process group.

If no PTY test harness exists, unit-test the formatting of
`StoppedByJobControl(SIGTTOU)`.

### Integration test: SIGPIPE forgiveness

Command:

```ral
yes | head -n 1
```

Expected:

- exit 0;
- stdout has one line;
- stderr empty.

This protects against replacing status 141 incorrectly.

### Integration test: real SIGKILL

Command:

```ral
sh -c "kill -KILL \$\$"
```

Expected:

- exit code 137;
- stderr contains `"killed by signal 9 (SIGKILL)"`;
- stderr does not contain `"stopped by signal"`.

This ensures real signal death is still reported as signal death.

### Integration test: normal exit 137

Command:

```ral
sh -c "exit 137"
```

Expected:

- exit code 137;
- stderr contains `"exited with status 137"`;
- stderr does not contain `"killed by signal 9"`.

This is essential. It proves normal exit code 137 and SIGKILL are no
longer conflated.

### TerminalPlan unit tests

Add pure tests around `plan_terminal` without spawning processes.

Cases:

1. all internal -> `NoTerminal`;
2. pure external -> `ForegroundExternalGroup`;
3. mixed in `_ed-tui` -> `ForegroundUnderEdTui`;
4. mixed, no known terminal program -> `MixedNoForeground`;
5. mixed with `glow -p` -> `RejectTerminalSensitiveMixed`;
6. mixed with `glow` without `-p` -> not rejected unless `glow` is
   considered always terminal-sensitive. Prefer not rejected initially.
7. mixed with `fzf` -> rejected.
8. pure external with `glow -p` -> `ForegroundExternalGroup`, not
   rejected.

### Integration test: reject known terminal tail in mixed pipeline

Avoid depending on `glow`. Use `fzf` only if installed in the container;
otherwise add a test-only fake command name to the classifier behind
`#[cfg(test)]`.

Better: unit-test the classifier and planner. Do not make CI depend on
optional terminal programs.

If using `glow` because it is installed in `shell-dev`, gate the test:

```rust
if command_missing("glow") {
    return;
}
```

Command:

```ral
to-lines ["a"] | glow -p
```

Expected:

- error before launch;
- stderr contains `"terminal program in a mixed pipeline"`;
- stderr does not contain `"SIGTTOU"`, `"SIGKILL"`, or `"137"`.

## Phase 11: docs

### SPEC.md

Update observable behavior only.

Add under failure propagation or process execution:

- normal non-zero exit is reported as an exit status;
- signal termination is reported as a signal;
- a stopped pipeline child is reported as stopped, and ral may kill the
  pipeline because it has no job control yet;
- non-final SIGPIPE is not a pipeline failure.

Add under pipeline/terminal behavior:

- pure external pipelines may own the terminal while running;
- mixed pipelines do not own the terminal by default;
- known terminal-consuming programs in mixed pipelines may be rejected
  before launch with a diagnostic;
- `_ed-tui` bodies are a special terminal-yielding context.

### RATIONALE.md

Add a short section:

```text
ral does not treat 137 as an explanation. A process may exit 137, die by
SIGKILL, or be killed by ral after a terminal-control stop. ral keeps
the structured cause until diagnostics are rendered, so users see the
event that matters.
```

Mention terminal-sensitive mixed pipelines plainly:

```text
An internal stage runs in ral. An interactive external program expects a
foreground process group. ral refuses known bad combinations rather than
letting the kernel stop the child and reporting a cleanup signal.
```

### IMPLEMENTATION.md

If this file has a pipeline runtime section, update it to mention:

- `WaitOutcome`;
- `CommandFailure`;
- `TerminalPlan`;
- SIGPIPE forgiveness by structured outcome.

## Phase 12: migration checklist

Before marking the work done, run these searches:

```sh
rg -n "128 \\+|status > 128|exited with status \\{.*137|SIGPIPE.*141|141" core ral exarch docs data
```

Allowed:

- docs explaining old behavior;
- tests asserting old strings are absent;
- comments in migration notes.

Not allowed in runtime code:

- `status > 128` signal inference;
- `code == 141` SIGPIPE forgiveness;
- `ExitStatus` conversion outside `WaitOutcome`.

Search:

```sh
rg -n "err\\.status" core ral exarch
```

Allowed:

- constructing `Error { status: ... }` inside `types/error.rs`;
- tests inspecting `Status`.

Prefer:

- `err.exit_code()`;
- `err.status_code_for_display()`;
- pattern matching on `Status` only in diagnostic-specific code.

Search:

```sh
rg -n "claim_foreground|plugin_context|in_tui|PipelineMode" core/src/evaluator/pipeline
```

Expected:

- `in_tui` appears in analysis/planning only;
- `claim_foreground` consumes `TerminalPlan`;
- `PipelineMode` may still exist for byte/value planning, but terminal
  foreground should not be decided from it in `group.rs`.

## Phase 13: recommended commit sequence

Keep commits small enough that tests can pass after each one.

1. Add `Signal` and `WaitOutcome`; no behavior change.
2. Return `WaitOutcome` from `wait_handling_stop`; migrate
   `RunningChild`.
3. Add `CommandFailure`; migrate standalone exec and pipeline process
   joins.
4. Add `Status`; migrate `Error` readers to `exit_code()`.
5. Remove `exit_code`, `external_exit_error`, and integer SIGPIPE
   checks.
6. Make `ExitHints` table-only.
7. Add `TerminalPlan`; preserve current terminal behavior at plan-time.
8. Add known terminal-sensitive classifier and reject mixed terminal
   tails before launch.
9. Migrate `ral/src/jobs.rs` wait status to `WaitOutcome`.
10. Add integration tests.
11. Update SPEC, RATIONALE, and IMPLEMENTATION.

If step 7 and step 8 are too large together, keep them separate. Step 7
should be a pure refactor. Step 8 changes behavior.

## Risks and decisions

### Risk: dependency cycle from Error to CommandFailure

Do not put `CommandFailure` under `evaluator` if `types::Error` needs to
store it and that creates a cycle. Move process outcome types to a lower
module.

Preferred final layout:

```text
core/src/process.rs              # Signal, WaitOutcome, CommandFailure
core/src/signal/unix.rs          # waitpid mechanics, returns WaitOutcome
core/src/evaluator/exec.rs       # process launch, consumes CommandFailure
core/src/types/error.rs          # Status::Process(CommandFailure)
```

### Risk: rejecting too many terminal programs

Keep the known terminal list conservative. `glow -p` and `fzf` are good
initial cases. Avoid broad guesses.

### Risk: making mixed pipelines unusable

Only reject mixed pipelines when a known terminal-sensitive external is
present. Existing mixed filter pipelines should behave the same.

### Risk: hiding cleanup SIGKILL completely

The main diagnostic should name the stop. The hint may say ral killed
the pipeline. Avoid saying "killed by SIGKILL" as the primary cause for
`StoppedThenKilled`.

### Risk: atomic redirect semantics

Current code commits atomic redirects on normal exit, even non-zero, and
does not commit on signal death. Preserve that:

- `WaitOutcome::Exited(_)` commits;
- `Signaled`, `StoppedThenKilled`, and `NativeCode` do not commit unless
  there is a documented platform reason.

### Risk: audit status changes

Audit currently records the effective status after SIGPIPE forgiveness.
Preserve that unless SPEC says audit should record raw status.

### Risk: OS exit code for stopped children

Use `128 + stopped_by`, not `128 + killed_by`, for
`StoppedThenKilled`. The stop is the meaningful user cause.

## Manual verification script

After implementation, run:

```sh
docker exec shell-dev cargo build 2>&1 | tail -20
docker exec shell-dev cargo test 2>&1 | tail -60
docker exec shell-dev ./target/debug/ral -c 'sh -c "exit 137"'
docker exec shell-dev ./target/debug/ral -c 'sh -c "kill -KILL \$\$"'
docker exec shell-dev ./target/debug/ral -c 'cat README.md | sh -c "kill -STOP \$\$"'
docker exec shell-dev ./target/debug/ral -c 'yes | head -n 1'
```

Expected summaries:

- `exit 137`: says exited with status 137; no SIGKILL hint.
- `kill -KILL`: says killed by signal 9 (SIGKILL).
- `kill -STOP`: says stopped by signal 19 (SIGSTOP); no primary
  "exited 137" message.
- `yes | head`: succeeds.

Manual PTY check outside non-interactive `-c`:

```sh
docker exec -it shell-dev ./target/debug/ral
ral $ cat README.md | glow -p
```

If the pipeline is pure external and `glow` is available, it should get
foreground normally. If the producer is internal, for example:

```ral
to-lines ["# title"] | glow -p
```

it should fail before launch with the terminal-program-in-mixed-pipeline
diagnostic.

## Definition of done

The work is complete only when all of these are true:

- no runtime code infers signals from `status > 128`;
- no runtime code forgives SIGPIPE by comparing to `141`;
- normal exit 137 and SIGKILL have different diagnostics;
- stopped-then-killed pipelines name the stop as the primary cause;
- known terminal-sensitive mixed pipelines are rejected before launch;
- pure external terminal pipelines still foreground;
- ordinary mixed non-terminal pipelines still work;
- `SPEC.md` and `RATIONALE.md` describe the observable behavior;
- all tests pass inside `shell-dev`.
