//! The external arm of command dispatch — `dispatch` picks it over env and
//! handler.  An external's life, read top to bottom: its head is resolved,
//! admitted and probed; its argv is vetted; [`prepare`] builds the launch; the
//! child's streams are wired and its group placed; [`start`] spawns it; the
//! pipeline collector, every external's one waiter, waits on it; [`ending`]
//! reads how it ended.  Each stage raises its failures one way.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::run`] — but a direct stage shares
//! [`prepare`], [`start`], the collector and [`ending`] with it, so both
//! paths resolve, confine, start, wait on and end a call the same way.

use crate::authority::guard::{Denial, check_exec, check_exec_name};
use crate::authority::policy::{Admitted, Baseline, GrantStack, Program};
use crate::authority::sandbox::{Image, Ownership, SandboxProjection};
use crate::error::{Error, io_message};
use crate::fact::{Failure, Reason, Unrunnable};
use crate::io::{Edge, OutFd, Sink, Source, Streams};
use crate::ir::CommandName;
use crate::path::{self, Candidate, CwdPath, Kind, failure_trace};
use crate::process::{
    CancelCause, Launch, Membership, POLL_SLICE, PgidPolicy, PreparedTerminalLoan, Scope, Spawned,
    StdioSpec, TerminalLoan, Unstarted, WaitOutcome,
};
use crate::shell::runtime::{Break, Context, Mooring, Settled, Shell, TerminalAccess, Value};
use crate::ty::RefusedArg;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[cfg(unix)]
use crate::authority::policy::{Flag, ShellVerb};
#[cfg(unix)]
use crate::fact::{AuditIo, Receipt};
#[cfg(unix)]
use crate::process::group::Survivor;
#[cfg(unix)]
use crate::shell::runtime::{CommandOrigin, HandlerLookup};

// ── heads: resolve, admit, probe ──────────────────────────────────────────────

/// A [`Head`] freezes the shown spelling and its [`Target`] without I/O.
/// [`Head::admit`] judges each candidate before it is looked at, and reads
/// 127/126 off that judged walk for [`validate_argv`] to report.
#[derive(Debug)]
pub(crate) struct Head<P = Target> {
    /// A path head is shown as typed, not folded by [`path::shown`].
    pub(crate) shown: String,
    pub(crate) program: P,
}

/// An admitted program, or the 127/126 error the judged walk ends in.
pub(crate) type Admission = Result<Admitted, Error>;

#[derive(Debug)]
pub(crate) enum Target {
    Tool(&'static str),
    /// A path head's launch path; `None` where it names none.
    Path(Option<PathBuf>),
    /// A bare name's `PATH` and cwd, frozen at dispatch.
    Search {
        path: Option<String>,
        cwd: CwdPath,
    },
}

pub(crate) struct Denied {
    pub(crate) at: String,
    denial: Box<Denial>,
}

impl Head {
    /// Freeze `name` against `ctx`, without consulting the disk.
    pub(crate) fn resolve(name: &CommandName, ctx: &Context) -> Self {
        match name {
            CommandName::Bare(bare) => {
                let shown = bare.to_string();
                let program = crate::uutils::bundled_name(std::ffi::OsStr::new(bare.as_ref()))
                    .map_or_else(
                        || Target::Search {
                            path: ctx.env_overrides().get_or_host("PATH"),
                            cwd: ctx.cwd().clone(),
                        },
                        Target::Tool,
                    );
                Self { shown, program }
            }
            CommandName::Path(path) => Self {
                shown: path.clone(),
                program: Target::Path(Some(launch_path(&PathBuf::from(path), ctx))),
            },
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => Self {
                    shown: path.shown(&home),
                    program: Target::Path(Some(launch_path(&path.expand(&home), ctx))),
                },
                None => Self {
                    shown: path.to_literal(),
                    program: Target::Path(None),
                },
            },
        }
    }

    /// Select the first admitted program, retaining the first non-absence.
    pub(crate) fn select(
        self,
        grants: &GrantStack,
        baseline: &Baseline,
    ) -> Result<Head<Admission>, Denied> {
        let Self { shown, program } = self;
        let program = match program {
            Target::Tool(tool) => check_exec(grants, baseline, &shown, Program::Tool(tool))
                .map(Ok)
                .map_err(|denial| Denied {
                    at: shown.clone(),
                    denial,
                }),
            Target::Path(Some(file)) => {
                match probe(file.clone(), CandidateKind::Path, &shown, grants, baseline)? {
                    Err(error) if cfg!(windows) && is_absent(&error) => {
                        let meant = path::WINDOWS_EXEC_EXTENSIONS.iter().find(|ext| {
                            matches!(
                                probe(
                                    path::with_appended_suffix(&file, ext),
                                    CandidateKind::Path,
                                    &shown,
                                    grants,
                                    baseline,
                                ),
                                Ok(Ok(_))
                            )
                        });
                        Ok(Err(match meant {
                            Some(ext) => error.with_hint(format!("did you mean '{shown}.{ext}'?")),
                            None => error,
                        }))
                    }
                    admission => Ok(admission),
                }
            }
            Target::Search { path, cwd } => settle(
                path::search(
                    &shown,
                    path.as_deref(),
                    Some(&cwd),
                    |candidate| match judge(
                        candidate,
                        CandidateKind::Search,
                        &shown,
                        grants,
                        baseline,
                    ) {
                        Ok(Ok(admitted)) => ControlFlow::Break(admitted),
                        Ok(Err(error)) => ControlFlow::Continue(Ok(error)),
                        Err(denied) => ControlFlow::Continue(Err(denied)),
                    },
                ),
                &shown,
            ),
            Target::Path(None) => Ok(Err(not_found(&shown))),
        }?;
        Ok(Head { shown, program })
    }

    /// Select under the live stack, recording a refusal against `mooring`.
    pub(crate) fn admit(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Head<Admission>> {
        let search = matches!(&self.program, Target::Search { .. });
        let shown = self.shown.clone();
        self.select(&shell.context.grants, shell.baseline())
            .map_err(|Denied { denial, .. }| {
                let refusal = shell.exec_refused(*denial, mooring);
                if search {
                    refusal.map_error(|e| {
                        e.with_hint(format!(
                            "'{shown}' is no program in any PATH directory the grant runs from"
                        ))
                    })
                } else {
                    refusal
                }
            })
    }
}

/// The searched program; failing every candidate, the first failure other
/// than absence, in `PATH` order.
fn settle(
    searched: Result<Admitted, Vec<Result<Error, Denied>>>,
    shown: &str,
) -> Result<Admission, Denied> {
    match searched {
        Ok(admitted) => Ok(Ok(admitted)),
        Err(failures) => failures
            .into_iter()
            .find(|failure| !failure.as_ref().is_ok_and(is_absent))
            .unwrap_or_else(|| Ok(not_found(shown)))
            .map(Err),
    }
}

#[derive(Clone, Copy)]
enum CandidateKind {
    Path,
    Search,
}

/// `arg0` walked, then judged.
fn probe(
    arg0: PathBuf,
    candidate_kind: CandidateKind,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Result<Admission, Denied> {
    Candidate::walk(arg0).map_or_else(
        || Ok(Err(not_found(shown))),
        |candidate| judge(candidate, candidate_kind, shown, grants, baseline),
    )
}

/// A walked candidate judged before its kind or error is looked at.
fn judge(
    candidate: Candidate,
    candidate_kind: CandidateKind,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Result<Admission, Denied> {
    let Candidate {
        arg0,
        spelled,
        walked,
    } = candidate;
    let located = match walked {
        Ok(located) => located,
        Err(error) => {
            if let Ok(trace) = failure_trace(&spelled, &error) {
                check_exec_name(grants, baseline, shown, &trace).map_err(|denial| Denied {
                    at: path::shown(&arg0),
                    denial,
                })?;
            }
            return Ok(Err(match (candidate_kind, error.kind()) {
                (_, ErrorKind::NotFound | ErrorKind::NotADirectory)
                | (CandidateKind::Search, _) => not_found(shown),
                (CandidateKind::Path, _) => {
                    unstartable(candidate_kind, shown, &arg0, unrunnable(&error))
                }
            }));
        }
    };
    let at = path::shown(&arg0);
    let admitted = check_exec(
        grants,
        baseline,
        shown,
        Program::File {
            arg0,
            located: Box::new(located),
        },
    )
    .map_err(|denial| Denied { at, denial })?;
    let Program::File { arg0, located } = admitted.program() else {
        return Ok(Ok(admitted));
    };
    if located.is_executable() {
        return Ok(Ok(admitted));
    }
    let error = match located.trace().kind() {
        Some(kind) if matches!(candidate_kind, CandidateKind::Path) || kind == Kind::File => {
            unstartable(
                candidate_kind,
                shown,
                arg0,
                if kind == Kind::Dir {
                    Unrunnable::Directory
                } else {
                    Unrunnable::NotExecutable
                },
            )
        }
        _ => not_found(shown),
    };
    Ok(Err(error))
}

fn not_found(shown: &str) -> Error {
    Error::new(Reason::process(shown, Failure::NotFound))
}

fn is_absent(error: &Error) -> bool {
    matches!(error.reason.failure(), Some(Failure::NotFound))
}

/// `arg0`, where the walk stopped, cannot run for `why`.  A path head
/// reports the name typed, a bare name the file the `PATH` walk found.
fn unstartable(kind: CandidateKind, shown: &str, arg0: &Path, why: Unrunnable) -> Error {
    let found = match kind {
        CandidateKind::Path => shown.to_owned(),
        CandidateKind::Search => path::shown(arg0),
    };
    Error::new(Reason::process(shown, Failure::not_runnable(found, why)))
}

/// What an `io::Error` met launching a program means: a refusal is
/// permission denied, anything else is the OS's own words.
fn unrunnable(e: &std::io::Error) -> Unrunnable {
    match e.kind() {
        ErrorKind::PermissionDenied => Unrunnable::PermissionDenied,
        _ => Unrunnable::Io(e.to_string()),
    }
}

/// Where the launcher's child finds `target`, a path head's native file:
/// anchored at the cwd it starts in, never read for a sigil again.  Off
/// Windows as written, the kernel walking it: a fold would judge a
/// different file than a `..` after a link, or a trailing `/`, reaches.
/// Win32 folds `..` lexically itself, and an absolute path keeps
/// `CreateProcessW` off the process cwd.
fn launch_path(target: &Path, ctx: &Context) -> PathBuf {
    if cfg!(windows) {
        return ctx.resolver().anchor(target).into_inner();
    }
    ctx.launch_cwd().join(target)
}

// ── vet ───────────────────────────────────────────────────────────────────────

/// Stringify `args`, refusing any shape the syscall boundary cannot carry.
fn validate_argv(cmd: &str, args: &[Value]) -> Settled<Vec<String>> {
    args.iter()
        .map(|arg| match reject_exec_arg(cmd, arg) {
            Some(refusal) => Err(refusal),
            None => Ok(arg.to_string()),
        })
        .collect()
}

/// Per-argument shape gate: nothing [`RefusedArg`] names can reach `execve(2)`,
/// so each refusal carries the idiom that lowers it.
///
/// The refused set is shared with the checker, which raises the same refusal as
/// a static error wherever an argument's type is concrete.  This is the backstop
/// for what polymorphism hid from it — a `$x` whose shape only the run knows.
fn reject_exec_arg(cmd: &str, arg: &Value) -> Option<Break> {
    let refusal = arg.heads().first().copied().and_then(RefusedArg::of_head)?;
    Some(
        Error::new(format!(
            "cannot pass {} to external command '{cmd}'",
            arg.type_name()
        ))
        .with_hint(refusal.remedy(cmd))
        .into(),
    )
}

// ── prepare ───────────────────────────────────────────────────────────────────

/// A vetted call built into its launch, not yet started.  `shown` is the
/// name diagnostics and audit report; the launch runs `image` with `args`.
pub(crate) struct Prepared {
    shown: String,
    args: Vec<String>,
    pub(crate) launch: Launch,
    image: Image,
    /// The `#!` interpreter `image` names, read with it: the parent's alone,
    /// to word a failure to start it.
    interpreter: Option<PathBuf>,
    /// The launch's own sandbox, where it has one.
    #[cfg(unix)]
    projection: Option<SandboxProjection>,
    /// Whether an OS sandbox or the guest jail confines the child.
    confined: bool,
}

/// Read `head`'s verdict, vet `args`, and build the launch under
/// `ownership`.
pub(crate) fn prepare(
    head: Head<Admission>,
    args: &[Value],
    ownership: Ownership,
    shell: &Shell,
) -> Settled<Prepared> {
    let Head { shown, program } = head;
    let admitted = program?;
    let args = validate_argv(&shown, args)?;
    Ok(build_launch(shown, admitted, args, ownership, shell)?)
}

/// Build `admitted`'s launch with `args` under `ownership` and the shell's
/// scoped env, cwd and limits; the launch keeps its [`Image`], never the
/// judged program.  Building waits on nothing a stop must end: what may be
/// stopped is [`start`]'s.
fn build_launch(
    shown: String,
    admitted: Admitted,
    args: Vec<String>,
    ownership: Ownership,
    shell: &Shell,
) -> Result<Prepared, Error> {
    // A guest jail is already the guest's sandbox: bwrap needs unprivileged
    // user namespaces and the guest boots with `user.max_user_namespaces = 0`.
    // The in-process guard in `Head::admit` runs either way.
    let jail = crate::process::jail::guest();
    let projection = jail
        .is_none()
        .then(|| SandboxProjection::of(&shell.context.grants, shell.baseline(), Some(&admitted)))
        .flatten();
    let confined = projection.is_some() || jail.is_some();
    let image = Image::of(&admitted);
    let interpreter = crate::authority::sandbox::interpreter_of(&admitted);
    // The judged object and its directory handle end here: a launch holds a
    // name, never the object it was admitted for.
    drop(admitted);
    // A `grant` body evaluates in this process unconfined; spawned children
    // are the only thing an OS sandbox reaches, and this is where it does.
    let mut launch = match projection.as_ref() {
        Some(projection) => crate::authority::sandbox::sandboxed_command(
            projection,
            &image,
            &args,
            ownership,
            shell.context.cwd(),
        )?,
        None => image.launch(&args)?,
    };
    apply_env(&mut launch, shell);
    #[cfg(unix)]
    launch.limit_session(shell.session.child_limits);
    // Hooks run in order: a grant's limits apply last and win.
    if shell.has_active_capabilities() {
        launch.limit_resources()?;
    }
    #[cfg(seccomp)]
    if let Some(jail) = jail {
        let plan = jail
            .plan(ownership, shell.context.grants.net())
            .map_err(crate::process::jail::refused)?;
        launch.apply_guest_jail(&plan)?;
    }
    Ok(Prepared {
        shown,
        args,
        launch,
        image,
        interpreter,
        #[cfg(unix)]
        projection,
        confined,
    })
}

/// Thread the shell's env overrides, logical cwd and `PWD` into the
/// child; strip dynamic-loader overrides under an active grant.  `current_dir`
/// is set unconditionally because `cd` moves shell state and leaves the process
/// cwd alone, so an inherited `getcwd(3)` would be the wrong directory.
fn apply_env(cmd: &mut Launch, shell: &Shell) {
    for (k, v) in shell.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.cwd();
    cmd.current_dir(&cwd);
    cmd.env("PWD", &cwd);
    // ral keeps no previous directory; an inherited `OLDPWD` names the
    // launcher's, and would mislead a `cd -` inside the child.
    cmd.env_remove("OLDPWD");
    if shell.has_active_capabilities() {
        // A loader hook makes an admitted binary run someone else's code, so
        // the grant's judgment about which program may run would mean nothing.
        // On macOS the stakes are higher still: a sandboxed external is a
        // re-exec of ral itself, and dyld honours these before `main` runs, so
        // an injected dylib would execute before the child enters Seatbelt.
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
        for var in dyld_vars(shell) {
            cmd.env_remove(var);
        }
    }
}

/// Every `DYLD_`-prefixed name the child would otherwise see, from this
/// process's environment and from the shell's own overrides.  The whole prefix
/// rather than dyld's current list: the loader owns that namespace, and a name
/// added by a future dyld must not become a hole.
fn dyld_vars(shell: &Shell) -> Vec<std::ffi::OsString> {
    const PREFIX: &str = "DYLD_";
    let inherited = crate::host::vars_os()
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| k.as_encoded_bytes().starts_with(PREFIX.as_bytes()));
    let overridden = shell
        .context
        .env_overrides()
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, _)| std::ffi::OsString::from(k));
    inherited.chain(overridden).collect()
}

// ── wiring and placement ──────────────────────────────────────────────────────

// Stdio routing for a spawned external child: wire stdin, stdout and stderr
// into the `Launch` from sources and sinks.  The call's redirects are
// already installed on the shell's by `shell::exec::redirect`, and a pipeline
// edge is just another source or sink.  A standalone external and a
// pipeline's group are placed by the one rule, `placement`.

/// What a child at the boundary reads of `shell`'s stdin.  A joined group's
/// source is its owner's choice and passes through; otherwise the terminal
/// survives only where `readable`, in ral's own group or one lent it, and any
/// other group of its own reads EOF.
pub(crate) fn boundary_stdin(shell: &Shell, readable: bool) -> &Source {
    match &shell.io.stdin {
        Source::Terminal if shell.session.stage.is_none() && !readable => &Source::Empty,
        stdin => stdin,
    }
}

/// Where a child's or a stage's stdout goes: a ral sink, or a pipeline edge
/// handed over whole, its write end and its fate.
pub(crate) enum ChildOut {
    Sink(Sink),
    Edge {
        writer: os_pipe::PipeWriter,
        edge: Arc<Edge>,
    },
}

/// The child's stdin is its source: `Empty` is `/dev/null`, a reader a
/// duplicate of it, and `Terminal` ral's own fd 0.
fn stdin_stdio(stdin: &Source) -> Settled<StdioSpec> {
    Ok(match stdin {
        Source::Empty => StdioSpec::Null,
        Source::Terminal => StdioSpec::Inherit,
        Source::Reader(r) => r
            .try_clone()
            .map_err(|e| Error::new(io_message("could not duplicate stdin", &e)))?
            .into(),
    })
}

#[allow(clippy::needless_pass_by_value, reason = "named for `map_err`")]
fn pipe_error(e: std::io::Error) -> Error {
    Error::new(io_message("pipe", &e))
}

/// Wire the child's stdin, stdout and stderr, returning the sinks its piped
/// fds must be pumped into.  A `File` or `Pipe` sink is handed to the child
/// directly; an edge's fate is dropped, the sentinel hearing the child's
/// writes in its place.
///
/// The one platform split: when both output streams share a destination, Unix
/// registers a `pre_exec` `dup2` that runs after the kernel wired fd 1, so
/// fd 2 follows fd 1 wherever it went; Windows, having no `pre_exec`, hands
/// the child a second handle to that destination.
///
/// Audit capture belongs elsewhere: it tees the shell's own sinks at dispatch
/// level in `shell::exec::capture::with_audit_capture`.
pub(crate) fn wire_stdio(
    command: &mut Launch,
    stdin: &Source,
    stdout: ChildOut,
    stderr: &Sink,
) -> Settled<Pumps<Sink>> {
    command.stdin(stdin_stdio(stdin)?);
    let (out, out_pump, joined) = match stdout {
        ChildOut::Sink(sink) => {
            let (out, pump) = sink.child_stdio(OutFd::Stdout).map_err(pipe_error)?;
            (out, pump, stderr.same_destination(&sink).then_some(sink))
        }
        ChildOut::Edge { writer, .. } => (StdioSpec::from_owned(writer), None, None),
    };
    command.stdout(out);
    let err_pump = if let Some(sink) = joined {
        join_stderr(command, &sink)?
    } else {
        let (err, pump) = stderr.child_stdio(OutFd::Stderr).map_err(pipe_error)?;
        command.stderr(err);
        pump
    };
    Ok(Pumps::new(out_pump, err_pump))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps, reason = "the Windows twin can fail")]
fn join_stderr(command: &mut Launch, _stdout: &Sink) -> Settled<Option<Sink>> {
    command.dup_stdout_to_stderr();
    Ok(None)
}

#[cfg(windows)]
fn join_stderr(command: &mut Launch, stdout: &Sink) -> Settled<Option<Sink>> {
    let (stdio, pump) = if matches!(stdout, Sink::Stdout) {
        // The child inherits our fd 1, so clone fd 1 — not fd 2 — for its
        // stderr.  The bare inherit would hand it fd 2, routing diagnostics
        // straight past `2>&1`.
        use std::os::windows::io::AsHandle;
        let owned = std::io::stdout()
            .as_handle()
            .try_clone_to_owned()
            .map_err(pipe_error)?;
        (StdioSpec::from_owned(owned), None)
    } else {
        stdout.child_stdio(OutFd::Stderr).map_err(pipe_error)?
    };
    command.stderr(stdio);
    Ok(pump)
}

/// A child's stdout/stderr pumps: planned as the [`Sink`] each drains into,
/// `None` where that fd is inherited or wired straight to an OS pipe, then
/// started as the drainer threads over the child's pipes.  A reader-gone kill's
/// remaining bytes are owed to nobody, and a descendant it missed could hold the
/// pipe for ever, so that ending detaches, as does every forced end; every
/// other joins, after the kill that frees the pipe.
pub(crate) struct Pumps<T = std::thread::JoinHandle<()>>(Streams<Option<T>>);

impl<T> Default for Pumps<T> {
    fn default() -> Self {
        Self(Streams::both(|| None))
    }
}

impl Pumps<Sink> {
    pub(crate) const fn new(stdout: Option<Sink>, stderr: Option<Sink>) -> Self {
        Self(Streams { stdout, stderr })
    }

    /// Start the planned drainers over `child`'s piped stdout/stderr, each
    /// reading `scope`, the launching computation's current scope.
    pub(crate) fn start(self, child: &mut crate::process::ChildHandle, scope: &Scope) -> Pumps {
        let Streams { stdout, stderr } = self.0;
        Pumps(Streams {
            stdout: stdout
                .and_then(|sink| child.take_stdout().map(|s| sink.pump(s, scope.clone()))),
            stderr: stderr
                .and_then(|sink| child.take_stderr().map(|s| sink.pump(s, scope.clone()))),
        })
    }
}

impl Pumps {
    /// Join both drainers a slice at a time; dropping `self` instead detaches
    /// them.  On a cause in force on `scope`, `kill` once: a pump a leader's
    /// survivor holds open ends only when the survivor's pipe closes.  On
    /// Windows, where a pipe write cannot be polled, the pumps are interrupted
    /// every slice too: one blocked on an edge a teardown left undrained would
    /// otherwise wait for a read end the collector holds until this very stage
    /// files.
    pub(crate) fn join_under(self, scope: &Scope, kill: impl FnOnce()) {
        let Streams { stdout, stderr } = self.0;
        let handles: Vec<_> = [stdout, stderr].into_iter().flatten().collect();
        let mut kill = Some(kill);
        while !handles.iter().all(std::thread::JoinHandle::is_finished) {
            if scope.is_cancelled() {
                if let Some(kill) = kill.take() {
                    kill();
                }
                #[cfg(windows)]
                handles.iter().for_each(crate::io::interrupt);
            }
            std::thread::sleep(POLL_SLICE);
        }
        for jh in handles {
            let _ = jh.join();
        }
    }
}

/// Where a lone command's child lands.
pub(crate) struct Placement {
    pub(crate) policy: PgidPolicy,
    /// Prepared before the spawn; lent the child's group once it exists.
    pub(crate) loan: Option<PreparedTerminalLoan>,
    /// Whether the terminal may be the child's stdin: it sits in ral's own
    /// group or in one the loan lends it.
    pub(crate) readable: bool,
}

/// A pipeline member's group: its pipeline's, unless an envelope, which leads
/// one of its own, stands between ral and the program.
pub(crate) fn member_policy(group: Membership, enveloped: bool) -> PgidPolicy {
    if enveloped {
        PgidPolicy::NewLeader
    } else {
        PgidPolicy::Join(group)
    }
}

/// The one decision of where a child lands and which terminal it is lent.
///
/// A stage's child is a member of the stage's group, whose owner holds any
/// loan.  Otherwise it takes the foreground, leading a group lent the
/// terminal, iff the run holds the session's terminal lease, its stdout is
/// terminal-bound or the run was lent the terminal explicitly, and it is not
/// `enveloped`: bwrap's `--new-session` puts a payload where `tcsetpgrp`
/// cannot name it, so an envelope leads a group of its own, unlent.
///
/// The lease, not `interactive`, says who owns the terminal: a script
/// launched at one foregrounds its children as the REPL does.  An explicit
/// loan's body (`_ed-tui`'s `fzf`) draws on `/dev/tty` whatever its stdout.
///
/// A headless child leads a session of its own, so a cancel kills its whole
/// subtree and nothing in it can signal the tty's owner; an interactive one
/// stays in ral's group, where the terminal's Ctrl-C reaches it.
pub(crate) fn placement(shell: &Shell, mooring: &Mooring, enveloped: bool) -> Placement {
    let interactive = shell.session.interactive;
    if let Some(stage) = shell.session.stage.as_ref() {
        return Placement {
            policy: member_policy(stage.clone(), enveloped),
            loan: None,
            readable: false,
        };
    }
    if enveloped {
        // A session of its own has no controlling terminal to stop a read.
        return Placement {
            policy: PgidPolicy::NewLeader,
            loan: None,
            readable: interactive,
        };
    }
    let foreground = matches!(shell.io.stdout, Sink::Stdout)
        || mooring.terminal_access == TerminalAccess::ExplicitLoan;
    match shell.terminal_lease(mooring).filter(|_| foreground) {
        Some(lease) => {
            let loan = TerminalLoan::prepare(lease, &mooring.home);
            Placement {
                policy: PgidPolicy::NewLeader,
                readable: loan.is_some(),
                loan,
            }
        }
        None => Placement {
            policy: if interactive {
                PgidPolicy::Inherit
            } else {
                PgidPolicy::NewSession
            },
            loan: None,
            readable: interactive,
        },
    }
}

// ── start ─────────────────────────────────────────────────────────────────────

/// A started call: its name and args, and what was born of it.
pub(crate) struct Started<T = Born> {
    pub(crate) shown: String,
    pub(crate) args: Vec<String>,
    pub(crate) born: T,
}

/// A kept child, and the deny lines its failure may claim.
pub(crate) struct Born {
    pub(crate) spawned: Spawned,
    pub(crate) claim: Claim,
}

/// The host pids whose kernel deny lines a child's failure owns.  A line
/// carries only the pid it denied, so each pid here was read while its
/// process lived: what died before, or was born after, goes unclaimed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// No sandbox confined the child: no line is its.
    Unconfined,
    /// A confined child's pids, as the spawn knew them.
    Confined(HashSet<u32>),
}

impl Claim {
    /// What a launch confined as `confined` claims of `spawned`, taken the
    /// moment it started: a direct child, its own pid; an enveloped one, the
    /// envelope's chain while its program runs, whose host pids no process
    /// inside the namespace can see.
    fn of(confined: bool, spawned: &Spawned) -> Self {
        if !confined {
            return Self::Unconfined;
        }
        #[cfg(target_os = "linux")]
        if let crate::process::Group::Enveloped(payload) = &spawned.group {
            return Self::Confined(crate::authority::sandbox::payload_pids(*payload));
        }
        Self::Confined(HashSet::from([spawned.child.id()]))
    }
}

/// Start `prepared` in `pgid` under `scope`, and claim what it bore.  The
/// one place a launch is stopped or fails as a program: its confinement is
/// applied, and its first word awaited, here.
pub(crate) fn start(prepared: Prepared, pgid: PgidPolicy, scope: &Scope) -> Settled<Started> {
    let confined = prepared.confined;
    started(prepared, |launch| {
        let spawned = launch.spawn(pgid, scope)?;
        let claim = Claim::of(confined, &spawned);
        Ok(Born { spawned, claim })
    })
}

/// Start `prepared` detached under `scope`, a survivor nothing here can
/// name again.
#[cfg(unix)]
fn start_detached(prepared: Prepared, scope: &Scope) -> Settled<Started<Survivor>> {
    started(prepared, |launch| launch.spawn_detached(scope))
}

/// `prepared`'s launch, spent by `spawn`.
fn started<T>(
    prepared: Prepared,
    spawn: impl FnOnce(Launch) -> Result<T, Unstarted>,
) -> Settled<Started<T>> {
    let Prepared {
        shown,
        args,
        launch,
        image,
        interpreter,
        #[cfg(unix)]
        projection,
        ..
    } = prepared;
    match spawn(launch) {
        Ok(born) => Ok(Started { shown, args, born }),
        Err(Unstarted::Refused(refusal)) => Err(refusal.into()),
        Err(Unstarted::Stopped(stop)) => Err(stop.into()),
        // The program's own failure, as the same process failure the head's
        // probe reports.
        Err(Unstarted::Program(e)) => {
            let withheld = || -> Option<String> {
                #[cfg(unix)]
                if let (Some(projection), Some(interpreter)) = (&projection, &interpreter) {
                    return crate::authority::sandbox::interpreter_hint(projection, interpreter);
                }
                None
            };
            Err(at_spawn(&e, &image, interpreter.as_deref(), withheld, &shown).into())
        }
    }
}

/// What `program`'s failure to start `image`, whose `#!` names `interpreter`,
/// means.  A script whose interpreter its sandbox withholds, as `withheld`
/// words it, is denied permission whether Seatbelt refused the exec or bwrap
/// hid the interpreter (`ENOENT`).  Otherwise `ENOENT` for a file still at
/// its real path is its missing interpreter or loader, and for one gone, or a
/// bundled tool, `program` not found.
fn at_spawn(
    e: &std::io::Error,
    image: &Image,
    interpreter: Option<&Path>,
    withheld: impl FnOnce() -> Option<String>,
    program: &str,
) -> Error {
    use crate::site::{Silent, site};
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    let failed = |why| {
        Error::new(Reason::process(
            program,
            Failure::not_runnable(program, why),
        ))
    };
    let denied = matches!(e.kind(), PermissionDenied | NotFound)
        .then(withheld)
        .flatten();
    match (denied, e.kind(), image) {
        (Some(hint), ..) => failed(Unrunnable::PermissionDenied).with_hint(hint),
        (None, NotFound, Image::File { real, .. })
            if site!(
                Silent::SpawnedImageStat,
                std::fs::symlink_metadata(real.as_path())
            )
            .is_ok() =>
        {
            failed(Unrunnable::MissingInterpreter(
                interpreter.map(crate::path::shown),
            ))
        }
        (None, NotFound, _) => Error::new(Reason::process(program, Failure::NotFound)),
        (None, ..) => failed(unrunnable(e)),
    }
}

// ── the ending ────────────────────────────────────────────────────────────────

/// How the run ended, from how its child did: a death by one of `cause`'s
/// signals is that cause's stop, success nothing, and any other end the
/// command's failure, with the kernel's account of the lines `claim` owns
/// since `since`.
pub(crate) fn ending(
    name: &str,
    outcome: WaitOutcome,
    cause: Option<CancelCause>,
    scope: &Scope,
    claim: &Claim,
    since: Instant,
) -> Settled<()> {
    if let Some(cause) = outcome.cancelled_by(cause) {
        return Err(Break::cancelled(cause, scope));
    }
    match outcome.failure() {
        None => Ok(()),
        Some(failure) => Err(with_denials(name, failure, claim, since).into()),
    }
}

/// `name`'s `failure` as its error, with, where a sandbox confined it, the
/// kernel's account: the deny lines since `since` that `claim` owns, else,
/// on macOS, the fork brake's for an exit status of 128.  An unconfined
/// failure reads no log.
fn with_denials(name: &str, failure: Failure, claim: &Claim, since: Instant) -> Error {
    let Claim::Confined(pids) = claim else {
        return Error::new(Reason::process(name, failure));
    };
    let exited = match failure {
        Failure::Exited(n) => Some(n),
        _ => None,
    };
    let hint = crate::authority::sandbox::denial_hint(pids, since, exited);
    #[cfg(target_os = "macos")]
    let hint = hint.or_else(|| exited.and_then(crate::process::limits::failure_hint));
    let error = Error::new(Reason::process(name, failure));
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

// ── run ───────────────────────────────────────────────────────────────────────

/// Runs a standalone external call from vetting to reap.  A bundled
/// coreutils/diffutils/ripgrep head takes the same path as any host
/// executable: its image is `ral --ral-bundled-tool <tool>`.
pub(crate) fn run(
    head: Head<Admission>,
    args: &[Value],
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    let mut prepared = prepare(head, args, Ownership::Kept, shell)?;
    let Placement {
        policy,
        loan: lending,
        readable,
    } = placement(shell, mooring, prepared.launch.enveloped());
    let pumps = wire_stdio(
        &mut prepared.launch,
        boundary_stdin(shell, readable),
        ChildOut::Sink(shell.io.stdout.clone()),
        &shell.io.stderr,
    )?;

    announce_command_title(&prepared.shown, shell);

    // Before the spawn, so a kernel deny the child logs falls inside the
    // window `denial_hint` reads back.
    let started = Instant::now();
    let Started { shown, born, .. } = start(prepared, policy, &shell.scope)?;
    // Its `Drop` restores ral's pgid on every path out of here, sparing the
    // next REPL tty read an EIO from a background pgroup.
    let loan = match lending {
        None => None,
        Some(lending) => match lending.lend(born.spawned.child.id().cast_signed()) {
            Some(loan) => Some(loan),
            None => return Err(unlent(born, pumps, shown, started, shell)),
        },
    };

    // Nothing fallible may run between `start` and here: until the collector
    // holds it the bare child leaks on an early return.
    super::pipeline::join_lone(born, pumps, loan, shown, started, shell).map(|()| Value::Unit)
}

/// A child whose terminal handoff failed is outside the foreground, where
/// its tty read stops it for good: killed and settled, and the refusal is the
/// verdict.  A stop in force stays so for the run's next poll.
fn unlent(born: Born, pumps: Pumps<Sink>, shown: String, started: Instant, shell: &Shell) -> Break {
    if let Some(group) = born.spawned.group.owned() {
        group.kill();
    }
    let refusal = Error::new(format!(
        "could not hand the terminal to '{shown}': tcsetpgrp refused its process group; the \
         command was killed before it could stop on a terminal read"
    ));
    let _ = super::pipeline::join_lone(born, pumps, None, shown, started, shell);
    refusal.into()
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.is_interactive() && shell.terminal().ui_controls_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::io::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

// ── detach ────────────────────────────────────────────────────────────────────

/// `detach <desc> <cmd> <args…>`: the double-forked survivor is nobody's to
/// signal, await or reap, so the result is a `{pid, desc}` receipt, and a
/// head a handler answers, which would run inside this session, is refused.
#[cfg(unix)]
pub(crate) fn detach(
    desc: &str,
    head: &Value,
    argv: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let name = CommandName::of(&head.to_string());
    if let CommandName::Bare(bare) = &name {
        // Every pass of the stack: a catch-all intercepts as a per-name does.
        match shell.lookup_handler(bare) {
            Some(HandlerLookup::Frame(..)) => {
                return Err(Error::new(format!(
                    "detach: `{bare}` is handled here (by an arm for it, or by the catch-all) \
                     and a handler runs inside this session, so nothing can be detached. To \
                     stub `detach`, stand in for `detach` itself; to run the real program, \
                     `^{bare}`"
                ))
                .into());
            }
            Some(HandlerLookup::Base(_)) => {
                return Err(Error::new(format!(
                    "detach: `{bare}` is ral's own, and runs inside this session; nothing \
                     can be detached"
                ))
                .into());
            }
            None => {}
        }
    }
    let head = Head::resolve(&name, &shell.context);
    let shown = head.shown.clone();
    let start = shell.audit_start(mooring);
    let launched = (|| -> Settled<Started<Survivor>> {
        shell.check(Flag::Shell(ShellVerb::Detach), "detach")?;
        // The grant's verdict on the program, then existence (127/126) and argv
        // shape: the same judgments an ordinary external passes, so a bundled
        // uutils tool falls out as its own image with no special case here.  The
        // survivor outlives this run, but its start is under this run's scope:
        // a stop before the fork makes no survivor; one while its trampoline
        // is silent leaves it.
        let head = head.admit(mooring, shell)?;
        let mut prepared = prepare(head, argv, Ownership::Surrendered, shell)?;
        prepared
            .launch
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        start_detached(prepared, &shell.scope)
    })();
    let (shown, args, result) = match launched {
        Ok(Started {
            shown,
            args,
            born: Survivor::Named(pid),
        }) => (
            shown,
            args,
            Ok(Value::from_datum(Receipt {
                pid,
                desc: desc.into(),
            })),
        ),
        Ok(Started {
            shown,
            args,
            born: Survivor::Unnamed(why),
        }) => {
            let lost = Error::new(format!(
                "detach: '{shown}' was started and is running, but its pid was lost ({why}); it cannot be stopped by pid"
            ));
            (shown, args, Err(lost.into()))
        }
        Err(broke) => (shown, Value::render_argv(argv), Err(broke)),
    };
    if let Some(start) = &start {
        let obs = shell.command_observation(
            start,
            &shown,
            args,
            CommandOrigin::Detached,
            result.as_ref().err(),
            AuditIo::default(),
        );
        shell.observe_stamped(Some(mooring), obs);
    }
    result
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
