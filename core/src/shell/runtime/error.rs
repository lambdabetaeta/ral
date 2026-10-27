//! The runtime error type.

use crate::compile::CompileError;
use crate::diagnostic::{Label, Rejection, Report, palette};
use crate::process::{CancelCause, CommandFailure, SpawnFailure, WorkerStop};
use crate::source::{CallSite, FileId, Source, SourceDb, Span};
use std::fmt::{self, Write as _};

mod record;
pub use record::outcome_value;

/// Rides in every `Settled`, so its optional parts are boxed: clippy's
/// `result_large_err` refuses an `Err` of 128 bytes.
#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: Status,
    /// `None` until `evaluator::machine`'s `stamp` sets the innermost
    /// enclosing node's span.
    pub span: Option<Span>,
    pub hint: Option<Box<str>>,
    /// A second place in the source the message points at: the use that imposed
    /// what a value failed to meet.
    pub witness: Option<Box<Span>>,
    /// The shown name of the command whose failure this is; `None` until
    /// `shell::runtime::flow::name_failure` stamps the innermost dispatch.
    pub(crate) command: Option<Box<str>>,
    /// A loaded file that failed to compile: renderers draw its report, not
    /// `message`.
    pub rejection: Option<Box<Rejection>>,
    /// The site a re-raised record carried, read by `record_at` ahead of the
    /// span.  Set only by `fail`.
    pub(crate) site: Option<Box<CallSite>>,
}

/// Said the same way wherever a block is sequenced: a phrase list
/// (`evaluator::run_phrases`) and a `Bind` chain (`evaluator::machine`).
const ABANDONED_TAIL_HINT: &str = "later steps in this block did not run; wrap a step in \
                                   `attempt` if its failure should not stop the rest";

const _: () = assert!(
    std::mem::size_of::<Error>() < 128,
    "Error must stay under clippy's result_large_err threshold"
);

/// An error's exit status: one constructor per fact, whichever door reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Raised(i32),
    /// A stop an owner converted into an error at its boundary.
    WorkerStopped(WorkerStop),
    Process(CommandFailure),
}

impl Status {
    /// The numeric status; each failure owns its code.
    pub fn code(&self) -> i32 {
        match self {
            Self::Raised(code) => *code,
            Self::Process(failure) => failure.code(),
            Self::WorkerStopped(owned) => i32::from(owned.cause().code()),
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

    /// A loaded file's failure: the plain message for a handler to read, the
    /// whole rejection for a renderer to draw.
    pub(crate) fn compile(error: CompileError, source: Source) -> Self {
        let message = error.to_string();
        Self {
            rejection: Some(Box::new(error.reject(source))),
            ..Self::new(message)
        }
    }

    pub fn raised(message: impl Into<String>, status: i32) -> Self {
        Self::with_status(message, Status::Raised(status))
    }

    pub(crate) fn with_status(message: impl Into<String>, status: Status) -> Self {
        Self {
            message: message.into(),
            status,
            span: None,
            hint: None,
            witness: None,
            command: None,
            rejection: None,
            site: None,
        }
    }

    /// An I/O failure on `what`, in the one phrasing every door shares.  The
    /// OS's errors and ral's own say the fixed string for `NotFound` and
    /// `PermissionDenied`, the same on every host; the exception is the
    /// directory's refusal of a staged write (`path::walk::directory_refused`),
    /// whose words say whose refusal it is.
    pub fn io(what: impl fmt::Display, e: &std::io::Error) -> Self {
        use std::io::ErrorKind::{NotFound, PermissionDenied};
        match e.kind() {
            NotFound => Self::new(format!("{what}: no such file or directory")),
            PermissionDenied if crate::path::walk::directory_refused(e) => {
                Self::new(format!("{what}: {e}"))
            }
            PermissionDenied => Self::new(format!("{what}: permission denied")),
            _ => Self::new(format!("{what}: {e}")),
        }
    }

    pub(crate) fn worker_stopped(owned: WorkerStop) -> Self {
        Self::with_status(owned.message(), Status::WorkerStopped(owned))
    }

    /// The diagnostic for a stop that reached the top of a run, drawn by the error
    /// renderer through a rendering-only `Error` that never leaves this fn.
    pub(crate) fn render_stop(
        cause: CancelCause,
        at: Option<Span>,
        db: &SourceDb,
        compact_root: Option<FileId>,
    ) -> String {
        Self {
            span: at,
            ..Self::raised(cause.message(), i32::from(cause.code()))
        }
        .render(db, compact_root)
    }

    /// A command that never became a process, whether the pre-spawn probe or
    /// the spawn itself found out.
    pub(crate) fn spawn_failure(cmd: &str, failure: SpawnFailure) -> Self {
        let hint = failure.text_hint(cmd, None);
        let failure = CommandFailure::Spawn(failure);
        Self {
            hint: hint.map(Into::into),
            ..Self::with_status(failure.message(cmd), Status::Process(failure))
        }
    }

    /// The error of a step that halted a sequence with steps after it.  A
    /// command that never became a process gets no advice to `attempt` it,
    /// which cannot make it run; a hint already carried says more.
    pub(crate) fn abandoning_tail(self) -> Self {
        match self.status {
            Status::Process(CommandFailure::Spawn(_)) => self,
            _ if self.hint.is_some() => self,
            _ => self.with_hint(ABANDONED_TAIL_HINT),
        }
    }

    /// A bare exit code takes its hint from the session's `exit_hints` table.
    pub(crate) fn from_command_failure(
        cmd: &str,
        failure: CommandFailure,
        shell: &crate::shell::runtime::Shell,
    ) -> Self {
        let hint = failure.default_hint().or_else(|| match &failure {
            CommandFailure::ExitCode(code) => shell.session.exit_hints.lookup(cmd, *code),
            _ => None,
        });
        Self {
            hint: hint.map(Into::into),
            ..Self::with_status(failure.message(cmd), Status::Process(failure))
        }
    }

    pub(crate) fn at_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into().into());
        self
    }

    /// Appends `more` below any hint already carried.
    pub(crate) fn append_hint(mut self, more: impl Into<String>) -> Self {
        let more = more.into();
        self.hint = Some(match self.hint.take() {
            Some(hint) => format!("{hint}\n\n{more}").into(),
            None => more.into(),
        });
        self
    }

    /// Prepends `more` above any hint already carried.
    pub(crate) fn prepend_hint(mut self, more: impl Into<String>) -> Self {
        let more = more.into();
        self.hint = Some(match self.hint.take() {
            Some(hint) => format!("{more}\n\n{hint}").into(),
            None => more.into(),
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
            hint: self.hint.as_deref().map(Into::into),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceFile, SourceOrigin};
    use crate::syntax::parser::{ParseError, ParseErrorKind};
    use std::io::{self, ErrorKind};

    fn file_origin(path: &str) -> SourceOrigin {
        SourceOrigin::File(SourceFile::new(path).expect("UTF-8"))
    }

    fn db_with(name: &str, text: &str) -> (SourceDb, FileId) {
        let mut db = SourceDb::default();
        let id = db.register(Source::from_text(file_origin(name), text));
        (db, id)
    }

    #[test]
    fn rejected_error_renders_its_compile_report() {
        let parse = ParseError {
            message: "expected ]".into(),
            span: Some(Span::new(FileId::DUMMY, 4, 5)),
            kind: ParseErrorKind::Plain { hint: None },
            incomplete: false,
        };
        let err = Error::compile(
            CompileError::Parse(parse),
            Source::from_text(file_origin("plugin.ral"), "let [x"),
        );
        let (db, root) = db_with("main.ral", "load-plugin plugin");
        let out = err.render(&db, Some(root));
        assert!(out.contains("P0001") && out.contains("plugin.ral"), "{out}");
        assert_eq!(err.message, "parse error: expected ]");
        assert_eq!(err.code(), 1);
        assert_eq!(err.rejection.as_ref().unwrap().status, 2);
    }

    fn runtime(
        db: &SourceDb,
        span: Option<Span>,
        witness: Option<Span>,
        hint: Option<&str>,
    ) -> String {
        let mut err = Error::new("boom").with_witness(witness);
        err.span = span;
        err.hint = hint.map(Into::into);
        err.render(db, None)
    }

    #[test]
    fn runtime_error_ariadne_points_at_source() {
        let (db, file) = db_with("test.al", "x = 5\ny = 10\necho $undefined\n");
        let output = runtime(&db, Some(Span::new(file, 18, 28)), None, None);
        assert!(output.contains("R0001"));
        assert!(output.contains("boom"));
        assert!(output.contains("test.al"));
    }

    #[test]
    fn runtime_error_ariadne_renders_hint() {
        let (db, file) = db_with("test.al", "[a, b] = 5");
        let output = runtime(
            &db,
            Some(Span::new(file, 0, 6)),
            None,
            Some("the right-hand side must evaluate to a list"),
        );
        assert!(output.contains("the right-hand side must evaluate to a list"));
    }

    #[test]
    fn runtime_error_resolved_source_draws_caret() {
        let (db, file) = db_with("main.ral", "echo x");
        let out = runtime(&db, Some(Span::new(file, 5, 6)), None, None);
        assert!(out.contains("R0001"));
        assert!(
            out.contains("here"),
            "a resolved span should draw a caret:\n{out}"
        );
        assert!(out.contains("main.ral"));
    }

    #[test]
    fn runtime_error_in_unregistered_source_is_spanless() {
        let (db, _) = db_with("main.ral", "echo x");
        let out = runtime(&db, Some(Span::new(FileId::DUMMY, 2, 6)), None, None);
        assert!(out.contains("R0001"));
        assert!(out.contains("boom"));
        assert!(
            !out.contains("here"),
            "an unresolved span must not draw a caret in any source:\n{out}"
        );
    }

    #[test]
    fn runtime_error_in_module_draws_into_module_source() {
        let mut db = SourceDb::default();
        let _top = db.register(Source::from_text(
            file_origin("main.ral"),
            "use 'mod.ral'\n",
        ));
        let module = db.register(Source::from_text(
            file_origin("mod.ral"),
            "let a = 1\nfail [status: 1, message: 'kaboom']\n",
        ));
        // Strip ANSI: colours may split `fail` character by character.
        let out =
            crate::io::ansi::strip(&runtime(&db, Some(Span::new(module, 10, 14)), None, None));
        assert!(out.contains("R0001"));
        assert!(
            out.contains("mod.ral"),
            "the caret must be drawn against the module's source:\n{out}"
        );
        assert!(
            !out.contains("main.ral"),
            "the top-level source must not appear:\n{out}"
        );
        assert!(
            out.contains("fail"),
            "the underlined line must be the module's line 2:\n{out}"
        );
    }

    fn said(e: &io::Error) -> String {
        Error::io("f", e).message
    }

    /// `NotFound` and `PermissionDenied` say the fixed string, whoever made
    /// them and whatever words they carry.
    #[test]
    fn io_says_the_fixed_string_for_not_found_and_permission_denied() {
        let os_not_found = io::Error::from_raw_os_error(2);
        assert_eq!(os_not_found.kind(), ErrorKind::NotFound);
        assert_eq!(said(&os_not_found), "f: no such file or directory");
        assert_eq!(
            said(&ErrorKind::PermissionDenied.into()),
            "f: permission denied"
        );
        assert_eq!(
            said(&ErrorKind::NotFound.into()),
            "f: no such file or directory"
        );
        assert_eq!(
            said(&io::Error::new(ErrorKind::PermissionDenied, "other words")),
            "f: permission denied"
        );
        assert_eq!(
            said(&io::Error::new(ErrorKind::NotFound, "other words")),
            "f: no such file or directory"
        );
    }

    /// The directory's refusal of a staged write says whose refusal it is.
    #[test]
    fn io_says_the_directorys_refusal_in_its_own_words() {
        assert_eq!(
            said(&crate::path::walk::directory_refusal_for_test()),
            "f: its directory is not writable"
        );
    }
}
