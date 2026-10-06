//! The runtime error type.

use crate::diagnostic::{Label, Rejection, Report, palette};
use crate::process::{CancelCause, ChildEnd, CommandFailure, SpawnFailure};
use crate::source::{FileId, SourceDb, Span};
use std::fmt::{self, Write as _};

mod record;
pub use record::outcome_value;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: Status,
    /// `None` until `evaluator::machine`'s `stamp` sets the innermost
    /// enclosing node's span.
    pub span: Option<Span>,
    pub hint: Option<String>,
    /// A second place in the source the message points at: the use that imposed
    /// what a value failed to meet.  Boxed, like `command`: `Error` rides in
    /// every `Settled`, and a larger one trips `result_large_err`.
    pub witness: Option<Box<Span>>,
    /// The shown name of the command whose failure this is; `None` until
    /// `types::flow::name_failure` stamps the innermost dispatch.
    pub(crate) command: Option<Box<str>>,
    /// A loaded file that failed to compile: renderers draw its report, not
    /// `message`.
    pub rejection: Option<Box<Rejection>>,
}

/// An error's exit status: one constructor per fact, whichever door reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Raised(i32),
    /// ral's own cancellation, whether a poll point or a child's teardown saw it.
    Cancelled(CancelCause),
    Process(CommandFailure),
}

impl Status {
    /// The numeric status; each failure owns its code.
    pub fn code(&self) -> i32 {
        match self {
            Self::Raised(code) => *code,
            Self::Process(failure) => failure.code(),
            Self::Cancelled(cause) => cause.code().into(),
        }
    }

    /// The code an external command chose to exit with, if that is what this is.
    pub fn exited(&self) -> Option<i32> {
        match self {
            Self::Process(CommandFailure::ExitCode(code)) => Some(*code),
            _ => None,
        }
    }
}

impl Error {
    /// An error with status 1.
    pub fn new(message: impl Into<String>) -> Self {
        Self::raised(message, 1)
    }

    pub fn raised(message: impl Into<String>, status: i32) -> Self {
        Self::with_status(message, Status::Raised(status))
    }

    fn with_status(message: impl Into<String>, status: Status) -> Self {
        Self {
            message: message.into(),
            status,
            span: None,
            hint: None,
            witness: None,
            command: None,
            rejection: None,
        }
    }

    /// An I/O failure on `what`, in the one phrasing every door shares.
    pub fn io(what: impl fmt::Display, e: &std::io::Error) -> Self {
        use std::io::ErrorKind::{NotFound, PermissionDenied};
        match e.kind() {
            NotFound => Self::new(format!("{what}: no such file or directory")),
            PermissionDenied => Self::new(format!("{what}: permission denied")),
            _ => Self::new(format!("{what}: {e}")),
        }
    }

    /// A poll point is any site that reads `CancelScope::cause()` (or
    /// `is_cancelled()`) and answers `Some` by ending the evaluation with an
    /// error; every such site mints that error through `Error::cancelled(cause)`.
    pub(crate) fn cancelled(cause: CancelCause) -> Self {
        Self::with_status(cause.message(), Status::Cancelled(cause))
    }

    pub(crate) fn cancelled_by(&self) -> Option<CancelCause> {
        match self.status {
            Status::Cancelled(cause) => Some(cause),
            _ => None,
        }
    }

    /// A child's end as the error `cmd` fails with; a cancelled one's is the
    /// very error a poll point mints for the same cause.
    pub(crate) fn of_child(cmd: &str, end: ChildEnd, shell: &crate::types::Shell) -> Self {
        match end {
            ChildEnd::Failed(failure) => Self::from_command_failure(cmd, failure, shell),
            ChildEnd::Cancelled(cause) => Self::cancelled(cause),
        }
    }

    /// A command that never became a process, whether the pre-spawn probe or
    /// the spawn itself found out.
    pub(crate) fn spawn_failure(cmd: &str, failure: SpawnFailure) -> Self {
        let failure = CommandFailure::Spawn(failure);
        Self::with_status(failure.message(cmd), Status::Process(failure))
    }

    /// A bare exit code takes its hint from the session's `exit_hints` table.
    pub(crate) fn from_command_failure(
        cmd: &str,
        failure: CommandFailure,
        shell: &crate::types::Shell,
    ) -> Self {
        let hint = failure.default_hint().or_else(|| match &failure {
            CommandFailure::ExitCode(code) => shell.session.exit_hints.lookup(cmd, *code),
            _ => None,
        });
        Self {
            hint,
            ..Self::with_status(failure.message(cmd), Status::Process(failure))
        }
    }

    pub(crate) fn at_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Appends `more` below any hint already carried.
    pub(crate) fn append_hint(mut self, more: impl Into<String>) -> Self {
        let more = more.into();
        self.hint = Some(match self.hint.take() {
            Some(hint) => format!("{hint}\n\n{more}"),
            None => more,
        });
        self
    }

    pub(crate) fn with_witness(mut self, witness: Option<Span>) -> Self {
        self.witness = witness.map(Box::new);
        self
    }

    /// Prefixes `message` with `ctx`, preserving span, hint, status and command.
    pub fn context(mut self, ctx: impl std::fmt::Display) -> Self {
        self.message = format!("{ctx}: {}", self.message);
        self
    }

    /// Numeric exit code for process exit and `$status`.
    pub fn code(&self) -> i32 {
        self.status.code()
    }

    /// A loaded file that failed to compile draws its own report.  Otherwise
    /// compact only when the error stayed inside `compact_root`'s file, the id
    /// of an input that compiled to a single command.
    ///
    /// Shape alone will not do: `boom` at the prompt is one command, but as an
    /// alias its error lives in the rc, where only a caret can point.
    ///
    /// Always ends in `\n`.  A caller that hands it straight to a sink
    /// expecting a trailing newline (`eprint!`, `write_all`, a wire payload)
    /// keeps it as is; one that stores it for a later `println!`/`eprintln!`
    /// must `trim_end()` first, or that newline doubles.
    pub fn render(&self, db: &SourceDb, compact_root: Option<FileId>) -> String {
        match (&self.rejection, compact_root) {
            (Some(rejection), _) => rejection.render(),
            (None, Some(root)) if self.span.is_none_or(|sp| sp.file == root) => self.compact(),
            _ => self.draw(db),
        }
    }

    /// The caret into the source `span` names, resolved through `db`.  A span
    /// `db` cannot resolve falls back to spanless: no caret beats one in the
    /// wrong file.
    fn draw(&self, db: &SourceDb) -> String {
        let label = |span, text: &str| Label {
            span: Some(span),
            text: text.into(),
        };
        let resolved = self.span.and_then(|sp| Some((sp, db.get(sp.file)?)));
        let report = |at, also| Report {
            code: Some("R0001"),
            message: self.message.clone(),
            at,
            also,
            hint: self.hint.clone(),
        };
        match resolved {
            Some((sp, src)) => report(
                Some(label(sp, "here")),
                self.witness
                    .as_deref()
                    .filter(|w| w.file == sp.file)
                    .map(|&w| label(w, "the use this script makes of it")),
            )
            .render(src),
            None => report(None, None).plain(),
        }
    }

    /// The one-liner for a single-command input, where a caret would only
    /// point back at the line the user just typed.
    pub fn compact(&self) -> String {
        let (red, cyan, reset) = palette();
        let mut out = format!("{red}error{reset}: {}", self.message);
        if let Some(code) = self.status_code_for_display() {
            let _ = write!(out, " (exit status {code})");
        }
        out.push('\n');
        if let Some(hint) = &self.hint {
            let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
        }
        out
    }

    /// `None` for a process failure, whose message already names its status.
    pub(crate) fn status_code_for_display(&self) -> Option<i32> {
        match &self.status {
            Status::Raised(0) | Status::Process(_) => None,
            status => Some(status.code()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}
