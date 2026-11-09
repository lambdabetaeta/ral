//! Running an external command.
//!
//! 1. [`Command::from_name`] records what to look up.
//! 2. [`Command::lookup`] finds the program and checks the grants.
//! 3. [`Command::admit`] reports a refusal. Other lookup errors wait for step 4.
//! 4. [`Command::prepare`] builds the launch ([`Prepared`]).
//! 5. [`Prepared::start`] spawns the child ([`Started`]).
//! 6. The caller collects the child, and [`ending`] reads its exit.

use super::job_control::Placement;
use crate::authority::guard::{Denial, check_exec, check_exec_name};
use crate::authority::policy::{Admitted, Baseline, GrantStack, Program};
use crate::authority::sandbox::{Confinement, Image, Ownership};
use crate::error::{Error, io_message};
use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
use crate::io::child::{Connection, Drains};
use crate::ir::CommandName;
use crate::path::{self, Candidate, CwdPath, Kind, failure_trace};
use crate::process::{CancelCause, Launch, PgidPolicy, Scope, Spawned, Unstarted, WaitOutcome};
use crate::shell::runtime::{Break, Context, Mooring, Settled, Shell, Value};
use crate::ty::RefusedArg;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Instant;

// The `detach` builtin, and the detached spawn only it may perform.
#[cfg(unix)]
mod detach;
#[cfg(unix)]
pub(in crate::shell) use detach::detach;

/// A command name, and how far it has got.
#[derive(Debug)]
pub(crate) struct Command<S> {
    /// The name used in messages.
    pub(crate) shown: String,
    state: S,
}

impl<S> Command<S> {
    pub(crate) fn state(&self) -> &S {
        &self.state
    }
}

// ── Lookup ──────────────────────────────────────────────────────────────

/// What to look up. Taken at dispatch; nothing touches the disk yet.
#[derive(Debug)]
pub(crate) enum Request {
    /// A tool built into ral.
    Tool(&'static str),
    /// A path, made absolute against the cwd.
    Path(PathBuf),
    /// A `~` path, but there is no home directory.
    NoHome,
    /// A bare name, searched for in PATH. PATH and cwd are copied now.
    Search { path: Option<String>, cwd: CwdPath },
}

/// The program found, or why none was.
pub(crate) type Lookup = Result<Admitted, LookupFailure>;

/// Why lookup failed.
pub(crate) enum LookupFailure {
    /// No such program. On Windows, may suggest an extension, e.g. `.exe`.
    NotFound {
        suggested_extension: Option<&'static str>,
    },
    NotRunnable(NotRunnable),
    /// A grant refused the program at `at`.
    Refused {
        at: String,
        denial: Box<Denial>,
    },
}

impl LookupFailure {
    const ABSENT: Self = Self::NotFound {
        suggested_extension: None,
    };
}

impl Command<Request> {
    /// Record what to look up.
    pub(crate) fn from_name(name: &CommandName, ctx: &Context) -> Self {
        match name {
            CommandName::Bare(bare) => Self {
                shown: bare.to_string(),
                state: crate::uutils::bundled_name(OsStr::new(bare.as_ref())).map_or_else(
                    || Request::Search {
                        path: ctx.env_overrides().get_or_host("PATH"),
                        cwd: ctx.cwd().clone(),
                    },
                    Request::Tool,
                ),
            },
            CommandName::Path(path) => Self {
                shown: path.clone(),
                state: Request::Path(ctx.resolver().for_launch(path)),
            },
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => Self {
                    shown: path.shown(&home),
                    state: Request::Path(ctx.resolver().for_launch(path.expand(&home))),
                },
                None => Self {
                    shown: path.to_literal(),
                    state: Request::NoHome,
                },
            },
        }
    }

    /// Find the program and check that the grants let it run. Reports
    /// nothing.
    pub(crate) fn lookup(self, grants: &GrantStack, baseline: &Baseline) -> Command<Lookup> {
        let Self { shown, state } = self;
        let state = match state {
            Request::Tool(tool) => check_exec(grants, baseline, &shown, Program::Tool(tool))
                .map_err(|denial| LookupFailure::Refused {
                    at: shown.clone(),
                    denial,
                }),
            Request::Path(file) => lookup_path(file, &shown, grants, baseline),
            Request::NoHome => Err(LookupFailure::ABSENT),
            Request::Search { path, cwd } => {
                path::search(&shown, path.as_deref(), Some(&cwd), |file| {
                    try_file(file, Origin::Searched, &shown, grants, baseline)
                        .map_or_else(ControlFlow::Continue, ControlFlow::Break)
                })
                .map_err(|failures| search_error(failures, &shown))
            }
        };
        Command { shown, state }
    }
}

/// Where a file's path came from.
#[derive(Clone, Copy)]
enum Origin {
    /// The user typed it.
    Typed,
    /// A PATH search built it from a bare name.
    Searched,
}

impl Origin {
    /// The name to show when the file cannot run.
    fn found(self, shown: &str, arg0: &Path) -> String {
        match self {
            Self::Typed => shown.to_owned(),
            Self::Searched => path::shown(arg0),
        }
    }

    /// Why a non-executable object of `kind` cannot run. `None` means
    /// treat it as missing: a PATH search skips anything that is not a file.
    fn unrunnable(self, kind: Option<Kind>) -> Option<Unrunnable> {
        match (self, kind?) {
            (_, Kind::File) => Some(Unrunnable::NotExecutable),
            (Self::Searched, _) => None,
            (Self::Typed, Kind::Dir) => Some(Unrunnable::Directory),
            (Self::Typed, _) => Some(Unrunnable::NotExecutable),
        }
    }
}

/// Look up a path the user typed. On Windows, if it is missing, suggest an
/// extension that would find it.
fn lookup_path(file: PathBuf, shown: &str, grants: &GrantStack, baseline: &Baseline) -> Lookup {
    #[cfg(windows)]
    let stem = file.clone();
    let lookup = Candidate::walk(file).map_or(Err(LookupFailure::ABSENT), |file| {
        try_file(file, Origin::Typed, shown, grants, baseline)
    });
    #[cfg(windows)]
    if matches!(lookup, Err(LookupFailure::NotFound { .. })) {
        let suggested_extension = path::with_exec_extensions(&stem).find_map(|(ext, file)| {
            try_file(file, Origin::Typed, shown, grants, baseline)
                .is_ok()
                .then_some(ext)
        });
        return Err(LookupFailure::NotFound {
            suggested_extension,
        });
    }
    lookup
}

/// Try one walked file as the program: first the grants, then whether it
/// can run.
/// The grants go first even if the walk failed, so a refused path never
/// reveals whether anything is there.
fn try_file(
    file: Candidate,
    origin: Origin,
    shown: &str,
    grants: &GrantStack,
    baseline: &Baseline,
) -> Lookup {
    let Candidate {
        arg0,
        spelled,
        walked,
    } = file;
    let at = path::shown(&arg0);
    let located = match walked {
        Ok(located) => located,
        Err(error) => {
            if let Ok(trace) = failure_trace(&spelled, &error) {
                check_exec_name(grants, baseline, shown, &trace)
                    .map_err(|denial| LookupFailure::Refused { at, denial })?;
            }
            return Err(match error.kind() {
                ErrorKind::NotFound | ErrorKind::NotADirectory => LookupFailure::ABSENT,
                _ => LookupFailure::NotRunnable(NotRunnable {
                    found: origin.found(shown, &arg0),
                    why: Unrunnable::from_io(&error),
                }),
            });
        }
    };
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
    // A handle can only be inspected once the grants have accepted it.
    let unrunnable = match admitted.program() {
        Program::File { arg0, located } if !located.is_executable() => {
            Some(match origin.unrunnable(located.trace().kind()) {
                Some(why) => LookupFailure::NotRunnable(NotRunnable {
                    found: origin.found(shown, arg0),
                    why,
                }),
                None => LookupFailure::ABSENT,
            })
        }
        _ => None,
    };
    unrunnable.map_or(Ok(admitted), Err)
}

/// Pick the error for a failed PATH search: the first one that is not
/// "missing". If every file was missing or simply not allowed, the refusal
/// gets a hint that no allowed PATH directory has the program.
fn search_error(failures: Vec<LookupFailure>, shown: &str) -> LookupFailure {
    let nowhere = failures.iter().all(|failure| match failure {
        LookupFailure::NotFound { .. } => true,
        LookupFailure::NotRunnable(_) => false,
        LookupFailure::Refused { denial, .. } => denial.missing_allowance,
    });
    match failures
        .into_iter()
        .find(|failure| !matches!(failure, LookupFailure::NotFound { .. }))
    {
        None => LookupFailure::ABSENT,
        Some(LookupFailure::Refused { at, mut denial }) if nowhere => {
            denial.error = denial.error.with_hint(format!(
                "'{shown}' is no program in any PATH directory the grant runs from"
            ));
            LookupFailure::Refused { at, denial }
        }
        Some(failure) => failure,
    }
}

// ── Admission ───────────────────────────────────────────────────────────

impl Command<Lookup> {
    /// Report a refusal at once.
    ///
    /// Other errors (not found, not runnable) are kept, and `prepare`
    /// reports them. By then the command's audit record has started and its
    /// redirects are open. These errors also beat bad arguments.
    pub(crate) fn admit(
        self,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Command<Result<Admitted, Error>>> {
        let Self { shown, state } = self;
        let state = match state {
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
        Ok(Command { shown, state })
    }
}

// ── Preparation ─────────────────────────────────────────────────────────

impl Command<Result<Admitted, Error>> {
    /// Change the kept lookup error, if there is one.
    pub(crate) fn map_lookup_error(mut self, map: impl FnOnce(Error) -> Error) -> Self {
        self.state = self.state.map_err(map);
        self
    }

    /// Report the kept lookup error, then any bad argument. Otherwise set up
    /// the sandbox, environment and limits. `ownership` says whether the
    /// session keeps the child. Cancellation is checked later, at start.
    pub(crate) fn prepare(
        self,
        args: &[Value],
        ownership: Ownership,
        shell: &Shell,
    ) -> Settled<Prepared> {
        let Self { shown, state } = self;
        let admitted = state?;
        let args = check_argv(&shown, args)?;
        let confinement = Confinement::of(&shell.context.grants, shell.baseline(), &admitted);
        let interpreter = crate::authority::sandbox::interpreter_of(&admitted);
        let image = Image::from(admitted);
        let mut launch = confinement.launch(&image, &args, ownership, shell.context.cwd())?;
        // Use the shell's cwd: ral's `cd` never calls `chdir`.
        launch.situate(
            shell
                .context
                .env_overrides()
                .for_child(shell.context.cwd(), shell.has_active_capabilities()),
        );
        #[cfg(unix)]
        launch.limit_session(shell.session.child_limits);
        // Grant limits apply after session limits.
        if shell.has_active_capabilities() {
            launch.limit_resources()?;
        }
        #[cfg(seccomp)]
        confinement.apply_guest_jail(&mut launch, ownership, shell.context.grants.net())?;
        Ok(Prepared {
            launch,
            diagnosis: Diagnosis {
                shown,
                image,
                interpreter,
                confinement,
            },
        })
    }
}

/// Turn the arguments into strings. Refuse values an external cannot take;
/// the type checker does the same, but cannot always know the type.
fn check_argv<'a>(cmd: &str, args: &'a [Value]) -> Settled<Vec<Cow<'a, str>>> {
    args.iter()
        .map(|arg| {
            if let Some(refused) = arg.heads().first().copied().and_then(RefusedArg::of_head) {
                return Err(Error::new(format!(
                    "cannot pass {} to external command '{cmd}'",
                    arg.type_name()
                ))
                .with_hint(refused.remedy(cmd))
                .into());
            }
            Ok(match arg {
                Value::String(text) => Cow::Borrowed(text.as_ref()),
                _ => Cow::Owned(arg.to_string()),
            })
        })
        .collect()
}

// ── Starting ────────────────────────────────────────────────────────────

/// A launch, ready except for its streams.
pub(crate) struct Prepared {
    launch: Launch,
    diagnosis: Diagnosis,
}

impl Prepared {
    /// Run the command on the shell's streams and wait for it.
    pub(crate) fn run(self, mooring: &Mooring, shell: &Shell) -> Settled<Value> {
        let placement = Placement::for_command(shell, mooring, self.enveloped());
        let io = Connection::new(
            placement.stdin(&shell.io.stdin),
            &shell.io.stdout,
            &shell.io.stderr,
        )
        .map_err(connection_error)?;
        announce_title(&self.diagnosis.shown, shell);
        // Kernel denials logged from here on may belong to this command.
        let since = Instant::now();
        self.start(io, placement.policy(), &shell.scope)?
            .collect(placement.into_loan(), since, shell)
            .map(|()| Value::Unit)
    }

    /// Whether the program runs inside an envelope. If so, place it with
    /// [`PgidPolicy::unless_enveloped`].
    pub(crate) fn enveloped(&self) -> bool {
        self.launch.enveloped()
    }

    /// Connect the streams, start the output drains, then spawn. If a drain
    /// fails to start, no child is spawned.
    pub(crate) fn start(self, io: Connection, pgid: PgidPolicy, scope: &Scope) -> Settled<Started> {
        let Self {
            mut launch,
            diagnosis,
        } = self;
        let drains = io.install(&mut launch).start(scope).map_err(|e| {
            Error::new(io_message(
                format!("could not start output drains for '{}'", diagnosis.shown),
                &e,
            ))
        })?;
        let spawned = launch
            .spawn(pgid, scope)
            .map_err(|why| diagnosis.explain_start(why))?;
        let denial_pids = diagnosis.denial_pids(&spawned);
        Ok(Started {
            shown: diagnosis.shown,
            spawned,
            denial_pids,
            drains,
        })
    }
}

#[allow(clippy::needless_pass_by_value, reason = "named for `map_err`")]
pub(super) fn connection_error(e: std::io::Error) -> Error {
    Error::new(io_message("could not connect command streams", &e))
}

/// Set the terminal title to the command, if allowed.
fn announce_title(cmd: &str, shell: &Shell) {
    if shell.is_interactive() && shell.terminal().ui_controls_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::io::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// A running child, waiting to be collected.
#[must_use = "collect or register the started command so its child is reaped"]
pub(crate) struct Started {
    pub(crate) shown: String,
    pub(crate) spawned: Spawned,
    /// The PIDs whose kernel sandbox denials belong to this child, or
    /// `None` if no sandbox confines it.
    pub(crate) denial_pids: Option<HashSet<u32>>,
    pub(super) drains: Drains,
}

// ── Failure to start ────────────────────────────────────────────────────

/// What we need to explain a failed start.
struct Diagnosis {
    shown: String,
    image: Image,
    /// The `#!` interpreter, read before the file handle was closed.
    interpreter: Option<PathBuf>,
    confinement: Confinement,
}

impl Diagnosis {
    /// Pass refusals and stops through. Explain the program's own `execve`
    /// error.
    fn explain_start(&self, why: Unstarted) -> Break {
        match why {
            Unstarted::Refused(refusal) => refusal.into(),
            Unstarted::Stopped(stop) => stop.into(),
            Unstarted::Program(e) => explain_exec_error(
                &e,
                &self.image,
                self.interpreter.as_deref(),
                || self.interpreter_hint(),
                &self.shown,
            )
            .into(),
        }
    }

    /// A hint if the sandbox hides the program's interpreter.
    #[cfg(unix)]
    fn interpreter_hint(&self) -> Option<String> {
        let interpreter = self.interpreter.as_deref()?;
        self.confinement.interpreter_hint(interpreter)
    }

    #[cfg(windows)]
    #[allow(clippy::unused_self, reason = "Windows programs name no interpreter")]
    fn interpreter_hint(&self) -> Option<String> {
        None
    }

    /// The PIDs to look for in the kernel's sandbox-denial log: the child's,
    /// or inside an envelope, the payload's host PIDs. Taken at spawn, while
    /// the processes are still alive.
    fn denial_pids(&self, spawned: &Spawned) -> Option<HashSet<u32>> {
        if !self.confinement.is_confined() {
            return None;
        }
        #[cfg(target_os = "linux")]
        if let crate::process::Group::Enveloped(payload) = &spawned.group {
            return Some(crate::authority::sandbox::payload_pids(*payload));
        }
        Some(HashSet::from([spawned.child.id()]))
    }
}

/// Explain why `execve` of `image` failed with `e`.
///
/// A sandbox that hides the interpreter gives EACCES or ENOENT, so `hint`
/// is only called for those. Otherwise, ENOENT for a file that still exists
/// means its interpreter or loader is missing.
fn explain_exec_error(
    e: &std::io::Error,
    image: &Image,
    interpreter: Option<&Path>,
    hint: impl FnOnce() -> Option<String>,
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
        .then(hint)
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

// ── Ending ──────────────────────────────────────────────────────────────

/// Turn a child's exit into a result. Killed by our own cancellation means
/// cancelled; any other failure gets sandbox-denial hints.
pub(crate) fn ending(
    name: &str,
    outcome: WaitOutcome,
    cause: Option<CancelCause>,
    scope: &Scope,
    denial_pids: Option<&HashSet<u32>>,
    since: Instant,
) -> Settled<()> {
    if let Some(cause) = outcome.cancelled_by(cause) {
        return Err(Break::cancelled(cause, scope));
    }
    match outcome.failure() {
        None => Ok(()),
        Some(failure) => Err(exit_error(name, failure, denial_pids, since).into()),
    }
}

/// The error for a failed exit, with a hint from kernel denials logged since
/// `since`. On macOS, fall back to resource-limit advice.
fn exit_error(
    name: &str,
    failure: Failure,
    denial_pids: Option<&HashSet<u32>>,
    since: Instant,
) -> Error {
    let Some(pids) = denial_pids else {
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
