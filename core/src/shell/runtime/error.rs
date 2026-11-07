//! The runtime error type.

use super::ExitHints;
use super::flow::Stop;
use crate::compile::CompileError;
use crate::diagnostic::{Label, Rejection, Report, palette};
use crate::fact::{Ending, Failure, FailureCode, Reason};
use crate::process::CancelCause;
use crate::source::{CallSite, FileId, Source, SourceDb, Span};
use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::num::NonZeroU8;
use std::sync::Arc;

mod record;
pub use record::outcome_value;

/// Rides in every `Settled`, so its optional parts are boxed: clippy's
/// `result_large_err` refuses an `Err` of 128 bytes.
#[derive(Debug, Clone)]
pub struct Error {
    pub reason: Reason,
    pub prov: Option<Prov>,
    /// Only advice that depends on more than the failure record.
    pub hint: Option<Box<str>>,
    pub witness: Option<Box<Span>>,
    pub rejection: Option<Box<Rejection>>,
    pub context: Option<Box<str>>,
    pub unwound: Unwound,
}

/// Where a failure was claimed.  `Live` is resolved against a source registry
/// only when drawn; `Resolved` carries its coordinates and renders no caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prov {
    Live { name: FrameName, span: Span },
    Resolved(Box<(String, CallSite)>),
}

/// A frame's name.  Static spellings and shared [`crate::ir::Name`]s clone
/// without allocating, which the evaluator's per-step register depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameName {
    Static(&'static str),
    Shared(Arc<str>),
}

impl std::ops::Deref for FrameName {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Static(name) => name,
            Self::Shared(name) => name,
        }
    }
}

impl From<&'static str> for FrameName {
    fn from(name: &'static str) -> Self {
        Self::Static(name)
    }
}

impl From<Arc<str>> for FrameName {
    fn from(name: Arc<str>) -> Self {
        Self::Shared(name)
    }
}

impl From<&Arc<str>> for FrameName {
    fn from(name: &Arc<str>) -> Self {
        Self::Shared(Arc::clone(name))
    }
}

impl From<Cow<'static, str>> for FrameName {
    fn from(name: Cow<'static, str>) -> Self {
        match name {
            Cow::Borrowed(name) => Self::Static(name),
            Cow::Owned(name) => name.into(),
        }
    }
}

impl From<String> for FrameName {
    fn from(name: String) -> Self {
        Self::Shared(name.into())
    }
}

impl Prov {
    pub fn name(&self) -> &str {
        match self {
            Self::Live { name, .. } => name,
            Self::Resolved(pair) => &pair.0,
        }
    }

    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Live { span, .. } => Some(*span),
            Self::Resolved(_) => None,
        }
    }

    pub(crate) fn site(&self, db: &SourceDb) -> CallSite {
        match self {
            Self::Live { span, .. } => db.site(*span).expect("live provenance is registered"),
            Self::Resolved(pair) => pair.1.clone(),
        }
    }

    /// Leaving the shell `db` belongs to: a span into a file the parent
    /// registry does not hold (or holds as another file) becomes coordinates.
    pub(crate) fn leaving(self, db: &SourceDb) -> Self {
        match self {
            Self::Live { name, span } if !db.inherits(span.file) => match db.site(span) {
                Some(site) => Self::Resolved(Box::new((name.to_string(), site))),
                None => Self::Live { name, span },
            },
            prov => prov,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Unwound {
    pub abandoned_tail: bool,
    pub discarded: u32,
}

/// Said the same way wherever a block is sequenced: a phrase list
/// (`evaluator::run_phrases`) and a `Bind` chain (`evaluator::machine`).
const ABANDONED_TAIL_HINT: &str = "later steps in this block did not run; wrap a step in \
                                   `attempt` if its failure should not stop the rest";

const _: () = assert!(
    std::mem::size_of::<Error>() < 128,
    "Error must stay under clippy's result_large_err threshold"
);

impl FailureCode for Error {
    fn failure_code(&self) -> NonZeroU8 {
        self.reason.failure_code()
    }
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self::raised(message)
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

    pub fn raised(message: impl Into<String>) -> Self {
        Self::from_reason(Reason::Raised(message.into()))
    }

    pub fn from_reason(reason: Reason) -> Self {
        Self {
            reason,
            prov: None,
            hint: None,
            witness: None,
            rejection: None,
            context: None,
            unwound: Unwound::default(),
        }
    }

    /// The failure's own provenance, else the register's frame.
    pub(crate) fn prov_or<'a>(&'a self, register: &'a Prov) -> &'a Prov {
        self.prov.as_ref().unwrap_or(register)
    }

    pub fn describe(&self) -> Cow<'_, str> {
        self.reason.describe()
    }

    /// An I/O failure on `what`, in the one phrasing every door shares.  The
    /// OS's errors and ral's own say the fixed string for `NotFound` and
    /// `PermissionDenied`, the same on every host; the exception is the
    /// directory's refusal of a staged write (`path::directory_refused`),
    /// whose words say whose refusal it is.
    pub fn io(what: impl fmt::Display, e: &std::io::Error) -> Self {
        use std::io::ErrorKind::{NotFound, PermissionDenied};
        match e.kind() {
            NotFound => Self::new(format!("{what}: no such file or directory")),
            PermissionDenied if crate::path::directory_refused(e) => {
                Self::new(format!("{what}: {e}"))
            }
            PermissionDenied => Self::new(format!("{what}: permission denied")),
            _ => Self::new(format!("{what}: {e}")),
        }
    }

    pub(crate) fn abandoning_tail(mut self) -> Self {
        self.unwound.abandoned_tail = true;
        self
    }

    pub(crate) fn discarded(mut self, bytes: u32) -> Self {
        self.unwound.discarded = self.unwound.discarded.saturating_add(bytes);
        self
    }

    /// A command that ended or never became a process.  A bare exit code
    /// takes its hint from the session's `exit_hints` table.
    pub(crate) fn process(program: &str, failure: Failure) -> Self {
        Self::from_reason(Reason::process(program, failure))
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

    pub fn context(mut self, ctx: impl std::fmt::Display) -> Self {
        self.context = Some(match self.context.take() {
            Some(inner) => format!("{ctx}: {inner}").into(),
            None => ctx.to_string().into(),
        });
        self
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
    pub fn render(&self, db: &SourceDb, compact_root: Option<FileId>, hints: &ExitHints) -> String {
        render(&self.diagnosis(hints), db, compact_root)
    }

    pub fn compact(&self, hints: &ExitHints) -> String {
        let diagnosis = self.diagnosis(hints);
        let (message, hint) = diagnosis.compose();
        diagnosis.compact(&message, hint.as_deref())
    }

    /// A host refusal has no source location.
    pub(crate) fn render_spanless(&self, hints: &ExitHints) -> String {
        render(
            &Diagnosis {
                at: None,
                witness: None,
                rejection: None,
                ..self.diagnosis(hints)
            },
            &SourceDb::default(),
            None,
        )
    }

    fn diagnosis(&self, hints: &ExitHints) -> Diagnosis<'_> {
        let hint = self
            .hint
            .as_deref()
            .map(Cow::Borrowed)
            .or_else(|| self.reason.hint())
            .or_else(|| {
                let Reason::Process(process) = &self.reason else {
                    return None;
                };
                let Failure::Exited(n) = process.failure else {
                    return None;
                };
                hints.lookup(&process.program, n).map(Cow::Owned)
            });
        Diagnosis {
            message: self.describe(),
            at: self.prov.as_ref(),
            witness: self.witness.as_deref().copied(),
            hint,
            shown: matches!(self.reason, Reason::TimedOut | Reason::Cancelled)
                .then(|| self.failure_code()),
            rejection: self.rejection.as_deref(),
            context: self.context.as_deref(),
            unwound: self.unwound,
            spawn: matches!(
                self.reason.failure(),
                Some(Failure::NotFound | Failure::NotRunnable(_))
            ),
        }
    }
}

struct Diagnosis<'a> {
    message: Cow<'a, str>,
    at: Option<&'a Prov>,
    witness: Option<Span>,
    hint: Option<Cow<'a, str>>,
    shown: Option<NonZeroU8>,
    rejection: Option<&'a Rejection>,
    context: Option<&'a str>,
    unwound: Unwound,
    spawn: bool,
}

impl Stop {
    fn diagnosis(&self) -> Diagnosis<'_> {
        Diagnosis {
            message: Cow::Borrowed(self.cause.message()),
            at: self.at.as_ref(),
            witness: None,
            hint: None,
            shown: Some(self.failure_code()),
            rejection: None,
            context: None,
            unwound: Unwound::default(),
            spawn: false,
        }
    }
}

impl<V> Ending<V, Error, Stop> {
    /// The one rendering: values, exits and reader-gone stops are silent.
    pub fn render(&self, db: &SourceDb, compact_root: Option<FileId>, hints: &ExitHints) -> String {
        match self {
            Self::Ok(_) | Self::Exited(_) => String::new(),
            Self::Err(error) => render(&error.diagnosis(hints), db, compact_root),
            Self::Stopped(stop) if stop.cause == CancelCause::ReaderGone => String::new(),
            Self::Stopped(stop) => render(&stop.diagnosis(), db, compact_root),
            Self::Panic(text) => render(
                &Diagnosis {
                    message: Cow::Owned(format!("run panicked: {text}")),
                    at: None,
                    witness: None,
                    hint: None,
                    shown: None,
                    rejection: None,
                    context: None,
                    unwound: Unwound::default(),
                    spawn: false,
                },
                db,
                None,
            ),
        }
    }
}

fn render(d: &Diagnosis<'_>, db: &SourceDb, compact_root: Option<FileId>) -> String {
    let (message, hint) = d.compose();
    let stays_in = |root| match d.at {
        Some(Prov::Live { span, .. }) => span.file == root,
        Some(Prov::Resolved(_)) | None => true,
    };
    if d.rejection.is_some() || compact_root.is_some_and(stays_in) {
        d.compact(&message, hint.as_deref())
    } else {
        d.draw(db, &message, hint.as_deref())
    }
}

impl Diagnosis<'_> {
    /// The words of a diagnosis before layout: the message under its context
    /// and resolved site, and the notes under it.
    fn compose(&self) -> (String, Option<String>) {
        let message = match self.context {
            Some(context) => format!("{context}: {}", self.message),
            None => self.message.to_string(),
        };
        let message = match self.at {
            Some(Prov::Resolved(pair)) => format!(
                "{}:{}:{}: {message}",
                pair.1.script, pair.1.line, pair.1.col
            ),
            _ => message,
        };
        let mut notes = self
            .hint
            .as_deref()
            .map(str::to_owned)
            .into_iter()
            .collect::<Vec<_>>();
        if self.unwound.discarded > 0 {
            notes.push(format!(
                "{} bytes written to stdout before the failure were discarded with the value; run the command without `let` to see them, or use `audit` to keep its output",
                self.unwound.discarded,
            ));
        }
        if self.unwound.abandoned_tail && !self.spawn && self.hint.is_none() {
            notes.push(ABANDONED_TAIL_HINT.into());
        }
        (message, (!notes.is_empty()).then(|| notes.join("\n\n")))
    }

    fn draw(&self, db: &SourceDb, message: &str, hint: Option<&str>) -> String {
        let label = |span, text: &str| Label {
            span: Some(span),
            text: text.into(),
        };
        let resolved = self
            .at
            .and_then(Prov::span)
            .and_then(|sp| Some((sp, db.get(sp.file)?)));
        let report = |at, also| Report {
            code: Some("R0001"),
            message: message.to_owned(),
            at,
            also,
            hint: hint.map(Into::into),
        };
        if let Some((sp, src)) = resolved {
            report(
                Some(label(sp, "here")),
                self.witness
                    .filter(|w| w.file == sp.file)
                    .map(|w| label(w, "the use this script makes of it")),
            )
            .render(src)
        } else {
            report(None, None).plain()
        }
    }

    /// One line and its hint; a rejection is already a report of its own.
    fn compact(&self, message: &str, hint: Option<&str>) -> String {
        let (red, cyan, reset) = palette();
        if let Some(rejection) = self.rejection {
            let mut out = match self.context {
                Some(context) => format!("{context}: {}", rejection.render()),
                None => rejection.render(),
            };
            if let Some(hint) = hint {
                let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
            }
            return out;
        }
        let mut out = format!("{red}error{reset}: {message}");
        if let Some(code) = self.shown {
            let _ = write!(out, " (exit status {code})");
        }
        out.push('\n');
        if let Some(hint) = hint {
            let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
        }
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.describe())
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::End;
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
    fn a_stop_renders_through_the_one_diagnosis() {
        let (db, file) = db_with("main.ral", "sleep 5");
        let end = End::Stopped(Stop {
            cause: CancelCause::TimedOut,
            owner: None,
            at: Some(Prov::Live {
                name: "sleep".into(),
                span: Span::new(file, 0, 7),
            }),
        });
        let caret = crate::io::ansi::strip(&end.render(&db, None, &ExitHints::default()));
        assert!(
            caret.contains("R0001") && caret.contains("here") && caret.contains("timed out"),
            "{caret}"
        );
        let compact = crate::io::ansi::strip(&end.render(&db, Some(file), &ExitHints::default()));
        assert!(compact.contains("timed out (exit status 124)"), "{compact}");
        assert!(!compact.contains("here"), "{compact}");
    }

    #[test]
    fn a_reader_gone_stop_renders_nothing() {
        let (db, file) = db_with("main.ral", "echo hi");
        let end = End::Stopped(Stop {
            cause: CancelCause::ReaderGone,
            owner: None,
            at: Some(Prov::Live {
                name: "echo".into(),
                span: Span::new(file, 0, 7),
            }),
        });
        assert_eq!(end.render(&db, None, &ExitHints::default()), "");
        assert_eq!(end.render(&db, Some(file), &ExitHints::default()), "");
    }

    #[test]
    fn a_panic_renders_spanless() {
        let (db, file) = db_with("main.ral", "echo hi");
        let out = crate::io::ansi::strip(&End::Panic("kaput".into()).render(
            &db,
            Some(file),
            &ExitHints::default(),
        ));
        assert!(out.contains("run panicked: kaput"), "{out}");
        assert!(!out.contains("here") && !out.contains("main.ral"), "{out}");
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
        )
        .discarded(412)
        .abandoning_tail()
        .context("use");
        let (db, root) = db_with("main.ral", "load-plugin plugin");
        let out = err.render(&db, Some(root), &ExitHints::default());
        assert!(out.contains("P0001") && out.contains("plugin.ral"), "{out}");
        assert_eq!(err.describe(), "parse error: expected ]");
        assert_eq!(err.failure_code(), NonZeroU8::MIN);
        assert_eq!(err.rejection.as_ref().unwrap().status.get(), 2);
        let discard = out.find("412 bytes written").expect("discard note");
        let tail = out.find("later steps").expect("abandoned tail note");
        assert!(
            out.find("P0001").unwrap() < discard && discard < tail,
            "{out}"
        );
        assert_eq!(out.matches("were discarded").count(), 1);
        assert_eq!(out.matches(ABANDONED_TAIL_HINT).count(), 1);

        let advised = err.with_hint("runtime advice");
        let out = advised.render(&db, Some(root), &ExitHints::default());
        assert!(out.contains("P0001"), "{out}");
        assert!(out.find("runtime advice").unwrap() < out.find("412 bytes written").unwrap());
        assert_eq!(out.matches("runtime advice").count(), 1);
        assert_eq!(out.matches("were discarded").count(), 1);
        assert!(!out.contains(ABANDONED_TAIL_HINT), "{out}");
    }

    fn runtime(
        db: &SourceDb,
        span: Option<Span>,
        witness: Option<Span>,
        hint: Option<&str>,
    ) -> String {
        let mut err = Error::new("boom").with_witness(witness);
        err.prov = span
            .filter(|sp| db.get(sp.file).is_some())
            .map(|span| Prov::Live {
                name: "fail".into(),
                span,
            });
        err.hint = hint.map(Into::into);
        crate::io::ansi::strip(&err.render(db, None, &ExitHints::default()))
    }

    #[test]
    fn runtime_error_ariadne_points_at_source() {
        let (db, file) = db_with("test.al", "x = 5\ny = 10\necho $undefined\n");
        let output = runtime(&db, Some(Span::new(file, 18, 28)), None, None);
        assert!(output.contains("R0001"));
        assert!(output.contains("test.al:3:6"), "{output}");
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
        assert!(output.contains("test.al:1:1"), "{output}");
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
        assert!(out.contains("main.ral:1:6"), "{out}");
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
            "let a = 1\nfail 'kaboom'\n",
        ));
        let out = runtime(&db, Some(Span::new(module, 10, 14)), None, None);
        assert!(out.contains("R0001"));
        assert!(
            out.contains("mod.ral:2:1"),
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

    #[test]
    fn unwind_facts_render_once_in_fixed_order_without_changing_the_record() {
        let shell = crate::test_helper::core_shell();
        let error = Error::new("boom")
            .discarded(200)
            .discarded(212)
            .abandoning_tail();
        let record = error.record(&shell);
        let rendered = error.render(shell.sources(), None, shell.exit_hints());
        let discard = rendered
            .find("412 bytes written")
            .expect("summed discard count");
        let tail = rendered.find("later steps").expect("abandoned tail");
        assert!(discard < tail, "{rendered}");
        assert_eq!(rendered.matches("were discarded").count(), 1);
        assert_eq!(record.describe(), "boom");
        let decorated = error
            .with_hint("specific advice")
            .context("inner")
            .context("outer");
        let rendered = decorated.render(shell.sources(), None, shell.exit_hints());
        assert!(rendered.contains("outer: inner: boom"), "{rendered}");
        assert!(
            rendered.find("specific advice").unwrap() < rendered.find("412 bytes written").unwrap()
        );
        assert!(!rendered.contains("later steps"), "{rendered}");
        assert_eq!(decorated.record(&shell), record);
        assert_eq!(decorated.discarded(u32::MAX).unwound.discarded, u32::MAX);
    }

    fn said(e: &io::Error) -> String {
        Error::io("f", e).describe().into_owned()
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
            said(&crate::path::directory_refusal_for_test()),
            "f: its directory is not writable"
        );
    }
}
