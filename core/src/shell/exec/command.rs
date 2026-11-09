//! Running an external command.
//!
//! 1. [`Command::from_name`] records what to look up.
//! 2. [`Command::lookup`] finds the program and checks the grants.
//! 3. [`Command::admit`] reports a refusal. Other lookup errors wait for step 4.
//! 4. [`Command::prepare`] builds the launch ([`Prepared`]).
//! 5. [`Prepared::start`] spawns the child ([`Started`]).
//! 6. The caller collects the child, and [`ending`] reads its exit.
//!
//! [`Command::run`] takes an admitted command alone through steps 4–6. A
//! pipeline stage and `detach` take those steps their own way.

use super::job_control::Placement;
use super::redirect::with_redirects;
use crate::authority::guard::{Denial, check_exec, check_exec_name};
use crate::authority::policy::{self, Admitted, Baseline, GrantStack};
use crate::authority::sandbox::{Confinement, Image, Ownership};
use crate::error::Error;
use crate::fact::{Failure, NotRunnable, Reason, Unrunnable};
use crate::io::child::{Connection, Drains};
use crate::ir::{CommandName, Redirects};
use crate::path::{self, Candidate, CwdPath, Kind, failure_trace};
use crate::process::{CancelCause, Launch, PgidPolicy, Scope, Spawned, Unstarted, WaitOutcome};
use crate::shell::runtime::{Break, CommandOrigin, Context, Mooring, Settled, Shell, Value};
use crate::source::Span;
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
    origin: Origin,
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

impl Request {
    /// How the program this request looks up was named.
    fn origin(&self) -> Origin {
        match self {
            Self::Tool(_) | Self::Search { .. } => Origin::Bare,
            Self::Path(_) | Self::NoHome => Origin::Path,
        }
    }
}

/// The program found, or why none was.
pub(crate) type Lookup = Result<Admitted, LookupFailure>;

/// Why lookup failed.
pub(crate) enum LookupFailure {
    Missing(Missing),
    /// A grant refused the program at `at`.
    Refused {
        at: String,
        denial: Box<Denial>,
    },
}

/// No program runs under the name. Reported once the run has begun.
pub(crate) enum Missing {
    /// Nothing is there. On Windows, may suggest an extension, e.g. `.exe`.
    NotFound {
        suggested_extension: Option<&'static str>,
    },
    /// A `~` path, but there is no home directory.
    NoHome,
    /// Something is there, but it cannot run.
    NotRunnable(NotRunnable),
}

impl LookupFailure {
    const ABSENT: Self = Self::Missing(Missing::NotFound {
        suggested_extension: None,
    });

    /// Nothing is there.
    fn is_absent(&self) -> bool {
        matches!(self, Self::Missing(Missing::NotFound { .. }))
    }
}

impl From<Missing> for LookupFailure {
    fn from(missing: Missing) -> Self {
        Self::Missing(missing)
    }
}

impl Missing {
    /// What is there is no program at all: a directory, or a file that is
    /// not executable.
    pub(crate) fn names_no_program(&self) -> bool {
        matches!(
            self,
            Self::NotRunnable(NotRunnable {
                why: Unrunnable::Directory | Unrunnable::NotExecutable,
                ..
            })
        )
    }

    /// The error reporting this for the command `shown`.
    fn into_error(self, shown: &str) -> Error {
        match self {
            Self::NotFound {
                suggested_extension,
            } => Error::new(Reason::process(shown, Failure::NotFound)).with_hints(
                suggested_extension.map(|ext| format!("did you mean '{shown}.{ext}'?")),
            ),
            Self::NoHome => Error::new(Reason::process(shown, Failure::NotFound))
                .with_hint("no home directory is set"),
            Self::NotRunnable(details) => {
                Error::new(Reason::process(shown, Failure::NotRunnable(details)))
            }
        }
    }
}

impl Command<Request> {
    /// Record what to look up.
    pub(crate) fn from_name(name: &CommandName, ctx: &Context) -> Self {
        let (shown, state) = match name {
            CommandName::Bare(bare) => (
                bare.to_string(),
                crate::uutils::bundled_name(OsStr::new(bare.as_ref())).map_or_else(
                    || Request::Search {
                        path: ctx.env_overrides().get_or_host("PATH"),
                        cwd: ctx.cwd().clone(),
                    },
                    Request::Tool,
                ),
            ),
            CommandName::Path(path) => {
                (path.clone(), Request::Path(ctx.resolver().for_launch(path)))
            }
            CommandName::TildePath(path) => match ctx.home() {
                Some(home) => (
                    path.shown(&home),
                    Request::Path(ctx.resolver().for_launch(path.expand(&home))),
                ),
                None => (path.to_literal(), Request::NoHome),
            },
        };
        Self {
            shown,
            origin: state.origin(),
            state,
        }
    }

    /// Find the program and check that the grants let it run. Reports
    /// nothing.
    pub(crate) fn lookup(self, grants: &GrantStack, baseline: &Baseline) -> Command<Lookup> {
        let Self {
            shown,
            origin,
            state,
        } = self;
        let state = match state {
            Request::Tool(tool) => {
                check_exec(grants, baseline, &shown, policy::Program::Tool(tool)).map_err(
                    |denial| LookupFailure::Refused {
                        at: shown.clone(),
                        denial,
                    },
                )
            }
            Request::Path(file) => lookup_path(file, &shown, grants, baseline),
            Request::NoHome => Err(Missing::NoHome.into()),
            Request::Search { path, cwd } => {
                path::search(&shown, path.as_deref(), Some(&cwd), |file| {
                    try_file(file, Origin::Bare, &shown, grants, baseline)
                        .map_or_else(ControlFlow::Continue, ControlFlow::Break)
                })
                .map_err(|failures| search_error(failures, &shown))
            }
        };
        Command {
            shown,
            origin,
            state,
        }
    }
}

/// How the user named a program.
#[derive(Debug, Clone, Copy)]
enum Origin {
    /// By a path.
    Path,
    /// By a bare name: a bundled tool, or a PATH search.
    Bare,
}

impl Origin {
    /// Where the program `shown` was found, at `arg0`: the path as typed, or
    /// the one a search built from a bare name.
    fn found(self, shown: &str, arg0: &Path) -> String {
        match self {
            Self::Path => shown.to_owned(),
            Self::Bare => path::shown(arg0),
        }
    }

    /// The program `shown`, found at `arg0`, cannot run for `why`.
    fn not_runnable(self, shown: &str, arg0: &Path, why: Unrunnable) -> Missing {
        Missing::NotRunnable(NotRunnable {
            found: self.found(shown, arg0),
            why,
        })
    }

    /// Why a non-executable object of `kind` cannot run. `None` means
    /// treat it as missing: a PATH search skips anything that is not a file.
    fn unrunnable(self, kind: Option<Kind>) -> Option<Unrunnable> {
        match (self, kind?) {
            (_, Kind::File) => Some(Unrunnable::NotExecutable),
            (Self::Bare, _) => None,
            (Self::Path, Kind::Dir) => Some(Unrunnable::Directory),
            (Self::Path, _) => Some(Unrunnable::NotExecutable),
        }
    }
}

/// Look up a path the user typed. On Windows, if it is missing, suggest an
/// extension that would find it.
fn lookup_path(file: PathBuf, shown: &str, grants: &GrantStack, baseline: &Baseline) -> Lookup {
    #[cfg(windows)]
    let stem = file.clone();
    let lookup = Candidate::walk(file).map_or(Err(LookupFailure::ABSENT), |file| {
        try_file(file, Origin::Path, shown, grants, baseline)
    });
    #[cfg(windows)]
    if lookup.as_ref().is_err_and(LookupFailure::is_absent) {
        let suggested_extension = path::with_exec_extensions(&stem).find_map(|(ext, file)| {
            try_file(file, Origin::Path, shown, grants, baseline)
                .is_ok()
                .then_some(ext)
        });
        return Err(Missing::NotFound {
            suggested_extension,
        }
        .into());
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
                _ => origin
                    .not_runnable(shown, &arg0, Unrunnable::from_io(&error))
                    .into(),
            });
        }
    };
    let admitted = check_exec(
        grants,
        baseline,
        shown,
        policy::Program::File {
            arg0,
            located: Box::new(located),
        },
    )
    .map_err(|denial| LookupFailure::Refused { at, denial })?;
    // A handle can only be inspected once the grants have accepted it.
    if let policy::Program::File { arg0, located } = admitted.program()
        && !located.is_executable()
    {
        return Err(origin
            .unrunnable(located.trace().kind())
            .map_or(LookupFailure::ABSENT, |why| {
                origin.not_runnable(shown, arg0, why).into()
            }));
    }
    Ok(admitted)
}

/// Pick the error for a failed PATH search: the first one that is not
/// "absent". If every file was absent or simply not allowed, the refusal
/// gets a hint that no allowed PATH directory has the program.
fn search_error(failures: Vec<LookupFailure>, shown: &str) -> LookupFailure {
    let nowhere = failures.iter().all(|failure| {
        failure.is_absent()
            || matches!(failure, LookupFailure::Refused { denial, .. } if denial.missing_allowance)
    });
    match failures.into_iter().find(|failure| !failure.is_absent()) {
        None => LookupFailure::ABSENT,
        Some(LookupFailure::Refused { at, mut denial }) if nowhere => {
            denial.error = denial.error.with_hint(format!(
                "'{shown}' is not in any PATH directory the grant allows"
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
    /// A missing program is kept, and `prepare` reports it. By then the
    /// command's audit record has started and its redirects are open. It
    /// also beats bad arguments.
    pub(crate) fn admit(
        self,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Command<Result<Admitted, Error>>> {
        let Self {
            shown,
            origin,
            state,
        } = self;
        let state = match state {
            Ok(admitted) => Ok(admitted),
            Err(LookupFailure::Missing(missing)) => Err(missing.into_error(&shown)),
            Err(LookupFailure::Refused { denial, .. }) => {
                return Err(shell.exec_refused(*denial, mooring));
            }
        };
        Ok(Command {
            shown,
            origin,
            state,
        })
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
        let Self {
            shown,
            origin,
            state,
        } = self;
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
            confinement,
            program: Program {
                shown,
                origin,
                image,
                interpreter,
            },
        })
    }
}

/// Turn the arguments into strings. Refuse values an external cannot take;
/// the type checker does the same, but cannot always know the type.
fn check_argv<'a>(cmd: &str, args: &'a [Value]) -> Result<Vec<Cow<'a, str>>, Error> {
    args.iter()
        .map(|arg| {
            if let Some(refused) = arg.heads().first().copied().and_then(RefusedArg::of_head) {
                return Err(Error::new(RefusedArg::headline(cmd, arg.type_name()))
                    .with_hint(refused.remedy(cmd)));
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
    confinement: Confinement,
    program: Program,
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
        .map_err(|e| self.connection_error(&e))?;
        if shell.is_interactive() {
            shell.terminal().set_title(&self.program.shown);
        }
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
            confinement,
            program,
        } = self;
        let drains = (io.install(&mut launch).start(scope))
            .map_err(|e| program.io_error("start output drains", &e))?;
        let spawned = launch
            .spawn(pgid, scope)
            .map_err(|why| program.unstarted(why, &confinement))?;
        let denial_pids = confinement.denial_pids(&spawned);
        Ok(Started {
            shown: program.shown,
            spawned,
            denial_pids,
            drains,
        })
    }

    /// The error for streams that could not be connected to this command.
    pub(super) fn connection_error(&self, e: &std::io::Error) -> Error {
        self.program.io_error("connect standard streams", e)
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

/// The program a launch starts, named as lookup named it. It outlives the
/// launch, which spawning consumes, to explain a failed start.
struct Program {
    shown: String,
    origin: Origin,
    image: Image,
    /// The `#!` interpreter, read before the file handle was closed.
    interpreter: Option<PathBuf>,
}

impl Program {
    /// Pass refusals and stops through. Explain the program's own `execve`
    /// error under `confinement`.
    fn unstarted(&self, why: Unstarted, confinement: &Confinement) -> Break {
        match why {
            Unstarted::Refused(refusal) => refusal.into(),
            Unstarted::Stopped(stop) => stop.into(),
            Unstarted::Program(e) => self
                .exec_error(&e, || {
                    let interpreter = self.interpreter.as_deref()?;
                    confinement.interpreter_hint(interpreter)
                })
                .into(),
        }
    }

    /// The I/O failure to `what` for this program.
    fn io_error(&self, what: &str, e: &std::io::Error) -> Error {
        Error::io(format!("{}: could not {what}", self.shown), e)
    }

    /// Where the program was found, as lookup names it.
    fn found(&self) -> String {
        match &self.image {
            Image::File { arg0, .. } => self.origin.found(&self.shown, arg0),
            Image::Tool(_) => self.shown.clone(),
        }
    }

    /// Explain why `execve` of this program failed with `e`.
    ///
    /// A sandbox that hides the interpreter gives EACCES or ENOENT, so
    /// `hint` is only called for those. Otherwise, ENOENT for a file that
    /// still exists means its interpreter or loader is missing.
    fn exec_error(&self, e: &std::io::Error, hint: impl FnOnce() -> Option<String>) -> Error {
        use crate::site::{Silent, site};
        use std::io::ErrorKind::{NotFound, PermissionDenied};
        let failed = |why| {
            let found = self.found();
            Error::new(Reason::process(
                &self.shown,
                Failure::not_runnable(found, why),
            ))
        };
        let denied = matches!(e.kind(), PermissionDenied | NotFound)
            .then(hint)
            .flatten();
        match (denied, e.kind(), &self.image) {
            (Some(hint), ..) => failed(Unrunnable::PermissionDenied).with_hint(hint),
            (None, NotFound, Image::File { real, .. })
                if site!(
                    Silent::SpawnedImageStat,
                    std::fs::symlink_metadata(real.as_path())
                )
                .is_ok() =>
            {
                failed(Unrunnable::MissingInterpreter(
                    self.interpreter.as_deref().map(crate::path::shown),
                ))
            }
            (None, NotFound, _) => Error::new(Reason::process(&self.shown, Failure::NotFound)),
            (None, ..) => failed(Unrunnable::from_io(e)),
        }
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
    let hint = denial_pids.and_then(|pids| {
        let exited = match failure {
            Failure::Exited(n) => Some(n),
            _ => None,
        };
        crate::authority::sandbox::denial_hint(pids, since, exited)
            .or_else(|| exited.and_then(crate::process::limits::failure_hint))
    });
    Error::new(Reason::process(name, failure)).with_hints(hint)
}

// ── Running alone ───────────────────────────────────────────────────────

impl Command<Result<Admitted, Error>> {
    /// Run as the `Exec` rule's external: in a frame of its own at `span`,
    /// recorded, under `redirects`, on the shell's streams, and waited for.
    pub(crate) fn run(
        self,
        args: &[Value],
        redirects: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Value> {
        let origin = CommandOrigin::External;
        self.with_record(args, origin, span, mooring, shell, |command, shell| {
            with_redirects(redirects, span, mooring, shell, |shell| {
                command
                    .prepare(args, Ownership::Kept, shell)?
                    .run(mooring, shell)
            })
        })
    }
}

impl Shell {
    /// `^program args…`, for a host holding no syntax: no handlers, no
    /// redirects, no span.
    ///
    /// # Errors
    /// Admission, argument, launch, child failure or cancellation.
    pub fn run_external(
        &mut self,
        program: &str,
        args: &[Value],
        mooring: &Mooring,
    ) -> Settled<Value> {
        Command::from_name(&CommandName::of(program), &self.context)
            .lookup(&self.context.grants, self.baseline())
            .admit(mooring, self)?
            .run(args, &Redirects::default(), None, mooring, self)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
