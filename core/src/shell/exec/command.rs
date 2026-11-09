//! External command lookup, preparation, and execution.
//!
//! [`Command<Request>`] snapshots the lookup context. Lookup returns a program
//! or a structured failure; admission handles policy refusals and produces
//! [`Command<Checked>`]. Ordinary lookup errors remain deferred until preparation,
//! where they take precedence over invalid arguments.
//!
//! Preparation builds an [`ExternalCommand`]. Connecting its streams consumes it
//! into a [`ConnectedCommand`], which owns the launch and its pump plan together.
//! Spawning produces [`Started`], collected either alone or by a pipeline. Both
//! use [`ending`] to interpret the child's result.

use crate::authority::guard::{Denial, check_exec, check_exec_name};
use crate::authority::policy::{Admitted, Baseline, GrantStack, Program};
use crate::authority::sandbox::{Confinement, Image, Ownership};
use crate::error::{Error, io_message};
use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
use crate::io::{Edge, OutFd, Sink, Source, Streams};
use crate::ir::CommandName;
use crate::path::{self, Candidate, CwdPath, Kind, failure_trace};
use crate::process::{
    CancelCause, Launch, POLL_SLICE, PgidPolicy, PreparedTerminalLoan, Scope, Spawned, StdioSpec,
    TerminalLoan, Unstarted, WaitOutcome,
};
use crate::shell::runtime::{Break, Context, Mooring, Settled, Shell, TerminalAccess, Value};
use crate::ty::RefusedArg;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[cfg(unix)]
use crate::process::group::Survivor;

// The detached-launch method is private. Other execution modules must use
// this child module's operation, which checks permission before calling it.
#[cfg(unix)]
mod detach;
#[cfg(unix)]
pub(in crate::shell) use detach::detach;

// Lookup and admission states.

/// A command name paired with its current state. Only admission constructs
/// [`Checked`], the state required for preparation.
#[derive(Debug)]
pub(crate) struct Command<S> {
    /// Diagnostic name. Explicit paths retain the user's spelling.
    pub(crate) shown: String,
    state: S,
}

/// Lookup inputs captured without filesystem access.
#[derive(Debug)]
pub(crate) enum Request {
    Tool(&'static str),
    /// An anchored path, or `None` for a tilde path without a home directory.
    Path(Option<PathBuf>),
    /// A bare name uses the PATH and cwd from dispatch, not later shell state.
    Search {
        path: Option<String>,
        cwd: CwdPath,
    },
}

/// The selected runnable program, or the lookup failure to report.
pub(crate) type Lookup = Result<Admitted, LookupFailure>;

/// Lookup retains failure data rather than inspecting rendered errors.
/// Policy refusals remain separate because admission must record them.
pub(crate) enum LookupFailure {
    NotFound {
        suggested_extension: Option<&'static str>,
    },
    NotRunnable(NotRunnable),
    Refused {
        at: String,
        denial: Box<Denial>,
    },
}

/// Admission has handled policy refusals. An ordinary error is retained so
/// preparation reports it before checking arguments.
#[derive(Debug)]
pub(crate) struct Checked {
    program: Result<Admitted, Error>,
}

// Capture, lookup, and admission.

impl Command<Request> {
    /// Capture the command name and its lookup context without filesystem access.
    pub(crate) fn from_name(name: &CommandName, ctx: &Context) -> Self {
        match name {
            CommandName::Bare(bare) => {
                let shown = bare.to_string();
                let state = crate::uutils::bundled_name(std::ffi::OsStr::new(bare.as_ref()))
                    .map_or_else(
                        || Request::Search {
                            path: ctx.env_overrides().get_or_host("PATH"),
                            cwd: ctx.cwd().clone(),
                        },
                        Request::Tool,
                    );
                Self { shown, state }
            }
            CommandName::Path(path) => Self {
                shown: path.clone(),
                state: Request::Path(Some(ctx.resolver().for_launch(path))),
            },
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => Self {
                    shown: path.shown(&home),
                    state: Request::Path(Some(ctx.resolver().for_launch(path.expand(&home)))),
                },
                None => Self {
                    shown: path.to_literal(),
                    state: Request::Path(None),
                },
            },
        }
    }

    /// Find an admitted runnable program. If none exists, report the first
    /// non-absence failure in PATH order.
    pub(crate) fn lookup(self, grants: &GrantStack, baseline: &Baseline) -> Command<Lookup> {
        let Self { shown, state } = self;
        let state = match state {
            Request::Tool(tool) => check_exec(grants, baseline, &shown, Program::Tool(tool))
                .map_err(|denial| LookupFailure::Refused {
                    at: shown.clone(),
                    denial,
                }),
            Request::Path(Some(file)) => {
                #[cfg(windows)]
                let file_for_hint = file.clone();
                match probe(file, CandidateKind::Path, &shown, grants, baseline) {
                    #[cfg(windows)]
                    Err(LookupFailure::NotFound { .. }) => {
                        let suggested_extension =
                            path::WINDOWS_EXEC_EXTENSIONS.iter().copied().find(|ext| {
                                probe(
                                    path::with_appended_suffix(&file_for_hint, ext),
                                    CandidateKind::Path,
                                    &shown,
                                    grants,
                                    baseline,
                                )
                                .is_ok()
                            });
                        Err(LookupFailure::NotFound {
                            suggested_extension,
                        })
                    }
                    lookup => lookup,
                }
            }
            Request::Search { path, cwd } => settle(
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
                        Ok(admitted) => ControlFlow::Break(admitted),
                        Err(failure) => ControlFlow::Continue(failure),
                    },
                ),
                &shown,
            ),
            Request::Path(None) => Err(LookupFailure::NotFound {
                suggested_extension: None,
            }),
        };
        Command { shown, state }
    }

    /// Apply the current grants. Record policy refusals immediately; convert
    /// ordinary lookup failures to errors for preparation to report.
    pub(crate) fn admit(self, mooring: &Mooring, shell: &mut Shell) -> Settled<Command<Checked>> {
        let Command { shown, state } = self.lookup(&shell.context.grants, shell.baseline());
        let program = match state {
            Ok(admitted) => Ok(admitted),
            Err(LookupFailure::NotFound {
                suggested_extension,
            }) => {
                let error = Error::new(Reason::process(&shown, Failure::NotFound));
                Err(match suggested_extension {
                    Some(ext) => error.with_hint(format!("did you mean '{shown}.{ext}'?")),
                    None => error,
                })
            }
            Err(LookupFailure::NotRunnable(details)) => Err(Error::new(Reason::process(
                &shown,
                Failure::NotRunnable(details),
            ))),
            Err(LookupFailure::Refused { denial, .. }) => {
                return Err(shell.exec_refused(*denial, mooring));
            }
        };
        Ok(Command {
            shown,
            state: Checked { program },
        })
    }
}

impl Command<Lookup> {
    /// Return the lookup result without recording refusals, for callers such
    /// as command inspection.
    pub(crate) fn into_result(self) -> Lookup {
        self.state
    }
}

// Candidate selection.

/// Select the first non-absence failure in PATH order. Add the missing-allowance
/// hint only when every candidate was absent or lacked an allowance. An explicit
/// exclusion or a non-runnable candidate prevents that hint.
fn settle(searched: Result<Admitted, Vec<LookupFailure>>, shown: &str) -> Lookup {
    let failures = match searched {
        Ok(admitted) => return Ok(admitted),
        Err(failures) => failures,
    };
    let unfound = failures.iter().all(|failure| match failure {
        LookupFailure::NotFound { .. } => true,
        LookupFailure::NotRunnable(_) => false,
        LookupFailure::Refused { denial, .. } => denial.missing_allowance,
    });
    match failures
        .into_iter()
        .find(|failure| !matches!(failure, LookupFailure::NotFound { .. }))
    {
        None => Err(LookupFailure::NotFound {
            suggested_extension: None,
        }),
        Some(LookupFailure::Refused { at, mut denial }) => {
            if unfound {
                denial.error = denial.error.with_hint(format!(
                    "'{shown}' is no program in any PATH directory the grant runs from"
                ));
            }
            Err(LookupFailure::Refused { at, denial })
        }
        Some(failure) => Err(failure),
    }
}

#[derive(Clone, Copy)]
enum CandidateKind {
    Path,
    Search,
}

impl CandidateKind {
    /// Explicit paths report the input spelling; PATH searches report the candidate.
    fn found(self, shown: &str, arg0: &Path) -> String {
        match self {
            Self::Path => shown.to_owned(),
            Self::Search => path::shown(arg0),
        }
    }
}

fn probe(
    arg0: PathBuf,
    candidate_kind: CandidateKind,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Lookup {
    Candidate::walk(arg0).map_or(
        Err(LookupFailure::NotFound {
            suggested_extension: None,
        }),
        |candidate| judge(candidate, candidate_kind, shown, grants, baseline),
    )
}

/// Check authority before reporting file type or lookup errors. Otherwise
/// inaccessible paths could reveal whether a program exists.
fn judge(
    candidate: Candidate,
    candidate_kind: CandidateKind,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Lookup {
    let Candidate {
        arg0,
        spelled,
        walked,
    } = candidate;
    let located = match walked {
        Ok(located) => located,
        Err(error) => {
            if let Ok(trace) = failure_trace(&spelled, &error) {
                check_exec_name(grants, baseline, shown, &trace).map_err(|denial| {
                    LookupFailure::Refused {
                        at: path::shown(&arg0),
                        denial,
                    }
                })?;
            }
            return Err(match error.kind() {
                ErrorKind::NotFound | ErrorKind::NotADirectory => LookupFailure::NotFound {
                    suggested_extension: None,
                },
                _ => LookupFailure::NotRunnable(NotRunnable {
                    found: candidate_kind.found(shown, &arg0),
                    why: Unrunnable::from_io(&error),
                }),
            });
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
    .map_err(|denial| LookupFailure::Refused { at, denial })?;
    let Program::File { arg0, located } = admitted.program() else {
        return Ok(admitted);
    };
    if located.is_executable() {
        return Ok(admitted);
    }
    let failure = match located.trace().kind() {
        Some(kind) if matches!(candidate_kind, CandidateKind::Path) || kind == Kind::File => {
            LookupFailure::NotRunnable(NotRunnable {
                found: candidate_kind.found(shown, arg0),
                why: if kind == Kind::Dir {
                    Unrunnable::Directory
                } else {
                    Unrunnable::NotExecutable
                },
            })
        }
        _ => LookupFailure::NotFound {
            suggested_extension: None,
        },
    };
    Err(failure)
}

// Argument validation.

/// Reject unsupported argument shapes, then borrow strings and render other
/// accepted values. The launch or sandbox payload owns the final argument bytes.
fn validate_argv<'a>(cmd: &str, args: &'a [Value]) -> Settled<Vec<Cow<'a, str>>> {
    args.iter()
        .map(|arg| match reject_exec_arg(cmd, arg) {
            Some(refusal) => Err(refusal),
            None => Ok(match arg {
                Value::String(text) => Cow::Borrowed(text.as_ref()),
                _ => Cow::Owned(arg.to_string()),
            }),
        })
        .collect()
}

/// Runtime counterpart of the type checker's [`RefusedArg`] check. Polymorphic
/// arguments may reach execution without a statically known shape.
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

// Launch preparation and stream connection.

/// A prepared launch with validated arguments, but no connected shell streams.
/// It owns the launch data; audit records still use the original argument values.
pub(crate) struct ExternalCommand {
    shown: String,
    launch: Launch,
    image: Image,
    /// Interpreter read from the admitted file before releasing its handle.
    /// Used only to explain startup errors.
    interpreter: Option<PathBuf>,
    confinement: Confinement,
}

/// A configured launch and the pumps for exactly that launch.
struct ConnectedCommand {
    command: ExternalCommand,
    pumps: Pumps<Sink>,
}

impl Command<Checked> {
    /// Attach context to a lookup error without reporting it early.
    pub(crate) fn map_lookup_error(mut self, map: impl FnOnce(Error) -> Error) -> Self {
        self.state.program = self.state.program.map_err(map);
        self
    }

    /// Prepare a launch whose child the session will own and collect.
    pub(crate) fn prepare(self, args: &[Value], shell: &Shell) -> Settled<ExternalCommand> {
        self.prepare_as(args, Ownership::Kept, shell)
    }

    /// Validate arguments, select confinement, and configure environment and
    /// limits. Cancellation is checked when starting, not while preparing.
    fn prepare_as(
        self,
        args: &[Value],
        ownership: Ownership,
        shell: &Shell,
    ) -> Settled<ExternalCommand> {
        let Self { shown, state } = self;
        let admitted = state.program?;
        let args = validate_argv(&shown, args)?;
        let confinement = Confinement::of(&shell.context.grants, shell.baseline(), &admitted);
        let interpreter = crate::authority::sandbox::interpreter_of(&admitted);
        // Image conversion releases the judged handles before launch.
        let image = Image::from(admitted);
        let mut launch = confinement.launch(&image, &args, ownership, shell.context.cwd())?;
        apply_env(&mut launch, shell);
        #[cfg(unix)]
        launch.limit_session(shell.session.child_limits);
        // Grant limits apply after session limits.
        if shell.has_active_capabilities() {
            launch.limit_resources()?;
        }
        #[cfg(seccomp)]
        confinement.apply_guest_jail(&mut launch, ownership, shell.context.grants.net())?;
        Ok(ExternalCommand {
            shown,
            launch,
            image,
            interpreter,
            confinement,
        })
    }

    /// Start a detached process with null streams. Return a survivor rather
    /// than a session-owned child, so the session cannot collect or kill it.
    #[cfg(unix)]
    fn detach(self, args: &[Value], shell: &Shell) -> Settled<Survivor> {
        let mut command = self.prepare_as(args, Ownership::Surrendered, shell)?;
        command
            .launch
            .stdin(StdioSpec::Null)
            .stdout(StdioSpec::Null)
            .stderr(StdioSpec::Null);
        command.launch.spawn_detached(&shell.scope).map_err(|why| {
            at_start(
                why,
                &command.image,
                command.interpreter.as_deref(),
                &command.confinement,
                &command.shown,
            )
        })
    }
}

impl ExternalCommand {
    /// Run and collect a standalone command. Set the terminal title only after
    /// connecting streams; the collector handles the terminal loan.
    pub(crate) fn run(self, mooring: &Mooring, shell: &Shell) -> Settled<Value> {
        let Placement {
            policy,
            loan,
            readable,
        } = placement(shell, mooring, self.launch.enveloped());
        let connected = self.connect(
            boundary_stdin(shell, readable),
            ChildOut::Sink(&shell.io.stdout),
            &shell.io.stderr,
        )?;
        announce_command_title(&connected.command.shown, shell);
        // Include startup in the interval used to attribute kernel denials.
        let since = Instant::now();
        connected
            .spawn(policy, &shell.scope)?
            .collect(loan, since, shell)
            .map(|()| Value::Unit)
    }

    /// Connect and spawn a pipeline stage. Its caller must register the result
    /// with the pipeline collector.
    pub(crate) fn start(
        self,
        stdin: &Source,
        stdout: ChildOut<&Sink>,
        stderr: &Sink,
        pgid: PgidPolicy,
        scope: &Scope,
    ) -> Settled<Started> {
        self.connect(stdin, stdout, stderr)?.spawn(pgid, scope)
    }

    /// Use descriptors for OS-backed streams and pumps for other destinations.
    /// Merge stderr when both outputs share a destination; Launch applies that
    /// merge after stdout configuration is complete.
    fn connect(
        mut self,
        stdin: &Source,
        stdout: ChildOut<&Sink>,
        stderr: &Sink,
    ) -> Settled<ConnectedCommand> {
        self.launch.stdin(
            stdin
                .child_stdio()
                .map_err(|e| Error::new(io_message("could not duplicate stdin", &e)))?,
        );
        let (out, out_pump, joined) = match stdout {
            ChildOut::Sink(sink) => {
                let (out, pump) = sink.child_stdio(OutFd::Stdout).map_err(pipe_error)?;
                (out, pump, stderr.same_destination(sink).then_some(sink))
            }
            ChildOut::Edge { writer, .. } => (StdioSpec::from_owned(writer), None, None),
        };
        self.launch.stdout(out);
        let err_pump = if let Some(sink) = joined {
            join_stderr(&mut self.launch, sink)?
        } else {
            let (err, pump) = stderr.child_stdio(OutFd::Stderr).map_err(pipe_error)?;
            self.launch.stderr(err);
            pump
        };
        Ok(ConnectedCommand {
            command: self,
            pumps: Pumps::new(out_pump, err_pump),
        })
    }
}

impl ConnectedCommand {
    /// Spawn once, transferring the child and its pump plan to the collector.
    fn spawn(self, pgid: PgidPolicy, scope: &Scope) -> Settled<Started> {
        let Self { command, pumps } = self;
        let pgid = pgid.unless_enveloped(command.launch.enveloped());
        let spawned = command.launch.spawn(pgid, scope).map_err(|why| {
            at_start(
                why,
                &command.image,
                command.interpreter.as_deref(),
                &command.confinement,
                &command.shown,
            )
        })?;
        let claim = Claim::of(&command.confinement, &spawned);
        Ok(Started {
            shown: command.shown,
            spawned,
            claim,
            pumps,
        })
    }
}

/// Apply the shell environment and cwd. The process cwd is not authoritative:
/// ral's `cd` changes shell state without calling `chdir`.
fn apply_env(cmd: &mut Launch, shell: &Shell) {
    for (k, v) in shell.context.env_overrides() {
        cmd.env(k, v);
    }
    let cwd = shell.context.cwd().as_path();
    cmd.current_dir(cwd);
    cmd.env("PWD", cwd);
    // ral does not track a previous directory; inherited OLDPWD would be stale.
    cmd.env_remove("OLDPWD");
    if shell.has_active_capabilities() {
        // Loader overrides can inject code into an admitted program. On macOS
        // they could run code in the re-exec before it enters Seatbelt.
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
        for var in dyld_vars(shell) {
            cmd.env_remove(var);
        }
    }
}

/// Loader overrides from both the inherited environment and shell overlays.
/// Match the entire prefix so newly introduced DYLD variables are also removed.
/// Inherited names own their snapshot; overlay names borrow the shell.
fn dyld_vars(shell: &Shell) -> impl Iterator<Item = Cow<'_, OsStr>> {
    const PREFIX: &str = "DYLD_";
    let inherited = crate::host::vars_os()
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| k.as_encoded_bytes().starts_with(PREFIX.as_bytes()))
        .map(Cow::Owned);
    let overridden = shell
        .context
        .env_overrides()
        .iter()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, _)| Cow::Borrowed(OsStr::new(k)));
    inherited.chain(overridden)
}

// Stream routing and terminal access.

/// Replace terminal stdin with EOF when the child cannot read it. Joined
/// pipeline stages retain the source selected by their owner; non-terminal
/// sources are never replaced.
pub(crate) fn boundary_stdin(shell: &Shell, readable: bool) -> &Source {
    match &shell.io.stdin {
        Source::Terminal if shell.session.stage.is_none() && !readable => &Source::Empty,
        stdin => stdin,
    }
}

/// A stage's output destination or an owned downstream pipe. `S` is `&Sink`
/// during wiring, or `()` while a pipeline defers borrowing the parent sink
/// until admission has finished mutating the shell.
pub(crate) enum ChildOut<S> {
    Sink(S),
    Edge {
        writer: os_pipe::PipeWriter,
        edge: Arc<Edge>,
    },
}

impl<S> ChildOut<S> {
    /// Resolve an existing destination without duplicating an owned pipe.
    pub(super) fn map_sink<T>(self, map: impl FnOnce(S) -> T) -> ChildOut<T> {
        match self {
            Self::Sink(sink) => ChildOut::Sink(map(sink)),
            Self::Edge { writer, edge } => ChildOut::Edge { writer, edge },
        }
    }
}

#[allow(clippy::needless_pass_by_value, reason = "named for `map_err`")]
fn pipe_error(e: std::io::Error) -> Error {
    Error::new(io_message("pipe", &e))
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
        // Inherit would select fd 2. Duplicate fd 1 instead to implement 2>&1.
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

/// Output pumps, represented by destinations before starting and join handles
/// afterwards. `None` means the stream uses a descriptor directly. Dropping
/// running pumps detaches them; collectors do this on forced termination to
/// avoid waiting indefinitely for descendants that still hold a pipe.
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

    /// Start output drainers using the command's cancellation scope.
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
    /// Wait for both drainers. On cancellation, call `kill` once to close pipes
    /// held by the child or its descendants. Windows also needs repeated I/O
    /// interruption: a drainer may be blocked writing to an undrained pipeline
    /// edge whose read end the collector still holds.
    pub(crate) fn join_under(self, scope: &Scope, kill: impl FnOnce()) {
        let Streams { stdout, stderr } = self.0;
        let handles = [stdout, stderr];
        let mut kill = Some(kill);
        while !handles
            .iter()
            .flatten()
            .all(std::thread::JoinHandle::is_finished)
        {
            if scope.is_cancelled() {
                if let Some(kill) = kill.take() {
                    kill();
                }
                #[cfg(windows)]
                handles.iter().flatten().for_each(crate::io::interrupt);
            }
            std::thread::sleep(POLL_SLICE);
        }
        for jh in handles.into_iter().flatten() {
            let _ = jh.join();
        }
    }
}

/// Process-group placement and terminal access for a launch.
pub(crate) struct Placement {
    pub(crate) policy: PgidPolicy,
    /// Prepared before spawning, then assigned to the child's process group.
    pub(crate) loan: Option<PreparedTerminalLoan>,
    /// Whether a standalone child may retain terminal stdin.
    pub(crate) readable: bool,
}

/// Choose process-group placement and terminal access.
///
/// Pipeline children join their stage's group. Standalone commands may receive
/// the terminal when the caller owns its lease and either writes to terminal
/// stdout or requests an explicit loan. A terminal-launched script can own a
/// lease too; `interactive` alone does not determine foreground access.
///
/// Envelopes cannot receive a terminal loan: bwrap puts the payload in another
/// session. Without a loan, interactive commands otherwise inherit ral's group
/// for Ctrl-C delivery; headless commands get a separate session for teardown.
pub(crate) fn placement(shell: &Shell, mooring: &Mooring, enveloped: bool) -> Placement {
    let interactive = shell.session.interactive;
    if let Some(stage) = shell.session.stage.as_ref() {
        return Placement {
            policy: PgidPolicy::Join(stage.clone()).unless_enveloped(enveloped),
            loan: None,
            readable: false,
        };
    }
    if enveloped {
        // The payload has no controlling terminal, so reads do not trigger SIGTTIN.
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

// Spawn results and failure attribution.

/// A session-owned child awaiting standalone collection or pipeline registration.
/// Detached launches never produce this type.
#[must_use = "collect or register the started command so its child is reaped"]
pub(crate) struct Started {
    pub(crate) shown: String,
    pub(crate) spawned: Spawned,
    pub(crate) claim: Claim,
    pub(super) pumps: Pumps<Sink>,
}

/// Host PIDs used to attribute kernel denial messages. Capture them during
/// startup while the processes are alive, rather than discovering PIDs after exit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Do not consult sandbox denial logs for this child.
    Unconfined,
    /// PIDs captured for a confined launch.
    Confined(HashSet<u32>),
}

impl Claim {
    /// Capture the direct child's PID, or the envelope payload's host PID chain.
    /// Processes inside the envelope cannot report their host-namespace PIDs.
    fn of(confinement: &Confinement, spawned: &Spawned) -> Self {
        if !confinement.is_confined() {
            return Self::Unconfined;
        }
        #[cfg(target_os = "linux")]
        if let crate::process::Group::Enveloped(payload) = &spawned.group {
            return Self::Confined(crate::authority::sandbox::payload_pids(*payload));
        }
        Self::Confined(HashSet::from([spawned.child.id()]))
    }
}

/// Preserve refusals and cancellation; explain OS startup errors using the
/// image and interpreter recorded during preparation.
fn at_start(
    why: Unstarted,
    image: &Image,
    interpreter: Option<&Path>,
    confinement: &Confinement,
    shown: &str,
) -> Break {
    match why {
        Unstarted::Refused(refusal) => refusal.into(),
        Unstarted::Stopped(stop) => stop.into(),
        Unstarted::Program(e) => {
            let withheld = || -> Option<String> {
                #[cfg(unix)]
                if let Some(interpreter) = interpreter {
                    return confinement.interpreter_hint(interpreter);
                }
                #[cfg(windows)]
                let _ = confinement;
                None
            };
            at_spawn(&e, image, interpreter, withheld, shown).into()
        }
    }
}

/// Distinguish missing programs from missing or withheld interpreters.
/// A sandbox may report a withheld interpreter as EACCES or ENOENT. Otherwise,
/// ENOENT with an existing image indicates a missing interpreter or loader.
/// Evaluate `withheld` only for errors that can carry that meaning.
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
        (None, ..) => failed(Unrunnable::from_io(e)),
    }
}

// Completion.

/// Interpret the child's result. A signal matching the cancellation cause is
/// cancellation; other failures may include sandbox diagnostics captured since
/// the launch began.
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

/// Add kernel denial messages for the launch's recorded PIDs and time interval.
/// On macOS, resource-limit advice is a fallback. Unconfined launches read no log.
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

/// Set the terminal title for an interactive command when UI controls are allowed.
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.is_interactive() && shell.terminal().ui_controls_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::io::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
