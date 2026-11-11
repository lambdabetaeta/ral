//! The rendering of a runtime error, and its minting as a record.

use super::ExitHints;
use super::flow::Stop;
use crate::diagnostic::{Label, Rejection, Report, palette};
use crate::error::{Error, Prov, Unwound, under_context};
use crate::fact::{Ending, Failure, FailureCode, Reason};
use crate::process::CancelCause;
use crate::source::{FileId, SourceDb, Span};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::num::NonZeroU8;

mod record;
pub use record::outcome_value;

/// Said the same way wherever a block is sequenced: a phrase list
/// (`evaluator::run_phrases`) and a `Bind` chain (`evaluator::machine`).
const ABANDONED_TAIL_HINT: &str = "later steps in this block did not run; wrap a step in \
                                   `attempt` if its failure should not stop the rest";

/// Intrinsic advice precedes the session's optional exit-table entry.
fn advice(reason: &Reason, exit_hints: &ExitHints) -> Vec<String> {
    let mut hints = reason.advice();
    if let Reason::Process(process) = reason
        && let Failure::Exited(code) = process.failure
        && let Some(hint) = exit_hints.lookup(&process.program, code)
    {
        hints.push(hint);
    }
    hints
}

fn context_advice(reason: &Reason, context: Option<&str>) -> Vec<String> {
    use crate::fact::{
        ContextFailure, ExarchFailure, HostFailure, IoKind, IoLeaf, IoOperation, IoSubject,
    };
    let Some(context) = context else {
        return Vec::new();
    };
    let advice = match reason {
        Reason::Host(host) => match (host.as_ref(), context) {
            (
                HostFailure::Exarch(ExarchFailure::Context(ContextFailure::EmptySelection)),
                "exarch-transcript `read",
            ) => "`!{range a b}` builds a run of ids",
            (
                HostFailure::Exarch(ExarchFailure::Context(ContextFailure::EmptySelection)),
                "exarch-transcript `grep",
            ) => "omit turns to search the whole transcript",
            (
                HostFailure::Exarch(ExarchFailure::Context(ContextFailure::CurrentlyWritten(_))),
                "exarch-transcript `read" | "exarch-transcript `grep",
            ) => "it is the one turn the transcript cannot read back yet",
            (
                HostFailure::Exarch(ExarchFailure::Context(ContextFailure::CurrentlyWritten(_))),
                "exarch-context `evict",
            ) => "an eviction keeps the work in hand",
            _ => return Vec::new(),
        }
        .to_owned(),
        Reason::Io(io) => {
            let IoSubject::Path(path) = &io.subject else {
                return Vec::new();
            };
            if !io.cause.context.is_empty()
                || !matches!(io.cause.leaf.as_ref(), IoLeaf::Os(os) if os.kind == IoKind::NotFound)
            {
                return Vec::new();
            }
            match (io.operation, context) {
                (IoOperation::Open, "view-text" | "view-hash" | "edit-hash" | "edit-replace") => {
                    format!(
                        "{path} does not exist; {context} never creates a file; `to-string BODY > path` writes a new one"
                    )
                }
                (
                    IoOperation::Walk,
                    "grep-files" | "grep-hidden" | "explore-dir" | "find-files",
                ) => format!(
                    "{path} does not exist; the path is a file or directory, relative to the cwd or absolute"
                ),
                _ => return Vec::new(),
            }
        }
        _ => return Vec::new(),
    };
    vec![advice]
}

impl Error {
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
    pub fn render(
        &self,
        db: &SourceDb,
        compact_root: Option<FileId>,
        exit_hints: &ExitHints,
    ) -> String {
        render(&self.diagnosis(exit_hints), db, compact_root)
    }

    pub fn compact(&self, exit_hints: &ExitHints) -> String {
        let diagnosis = self.diagnosis(exit_hints);
        let (message, notes) = diagnosis.compose(None);
        diagnosis.compact(&message, &notes)
    }

    /// The report's words, uncoloured and unlaid: the message line under its
    /// context, then its notes.  For a host's log or a command line's last
    /// word, where no session renders.
    #[must_use]
    pub fn plain(&self) -> String {
        let (mut out, notes) = self.diagnosis(&ExitHints::default()).compose(None);
        for hint in notes {
            out.push('\n');
            let _ = write!(out, "hint: {hint}");
        }
        out
    }

    /// A host refusal has no source location.
    pub(crate) fn render_spanless(&self, exit_hints: &ExitHints) -> String {
        render(
            &Diagnosis {
                at: None,
                witness: None,
                rejection: None,
                ..self.diagnosis(exit_hints)
            },
            &SourceDb::default(),
            None,
        )
    }

    fn diagnosis(&self, exit_hints: &ExitHints) -> Diagnosis<'_> {
        let contextual = self.hints().iter().map(|hint| Cow::Borrowed(&**hint));
        let intrinsic = advice(&self.reason, exit_hints).into_iter().map(Cow::Owned);
        let hints = if matches!(self.reason, Reason::Denied(_)) {
            intrinsic.chain(contextual).collect()
        } else {
            contextual
                .chain(
                    context_advice(&self.reason, self.context())
                        .into_iter()
                        .map(Cow::Owned),
                )
                .chain(intrinsic)
                .collect()
        };
        Diagnosis {
            message: self.describe(),
            at: self.prov.as_ref(),
            witness: self.witness(),
            hints,
            shown: matches!(self.reason, Reason::TimedOut | Reason::Cancelled)
                .then(|| self.failure_code()),
            rejection: self.rejection(),
            context: self.context(),
            unwound: self.unwound(),
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
    hints: Vec<Cow<'a, str>>,
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
            hints: Vec::new(),
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
    pub fn render(
        &self,
        db: &SourceDb,
        compact_root: Option<FileId>,
        exit_hints: &ExitHints,
    ) -> String {
        match self {
            Self::Ok(_) | Self::Exited(_) => String::new(),
            Self::Err(error) => render(&error.diagnosis(exit_hints), db, compact_root),
            Self::Stopped(stop) if stop.cause == CancelCause::ReaderGone => String::new(),
            Self::Stopped(stop) => render(&stop.diagnosis(), db, compact_root),
            Self::Panic(text) => render(
                &Diagnosis {
                    message: Cow::Owned(format!("run panicked: {text}")),
                    at: None,
                    witness: None,
                    hints: Vec::new(),
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
    let (message, notes) = d.compose(Some(db));
    let stays_in = |root| match d.at {
        Some(Prov::Live { span, .. }) => span.file == root,
        Some(Prov::Resolved(_)) | None => true,
    };
    if d.rejection.is_some() || compact_root.is_some_and(stays_in) {
        d.compact(&message, &notes)
    } else {
        d.draw(db, &message, &notes)
    }
}

impl Diagnosis<'_> {
    /// The words of a diagnosis before layout: the message under its context
    /// and resolved site, and the notes under it.
    fn compose(&self, db: Option<&SourceDb>) -> (String, Vec<String>) {
        let intrinsic = match self.witness.and_then(|witness| db?.site(witness)) {
            Some(site) => Cow::Owned(format!("{}, line {}", self.message, site.line)),
            None => self.message.clone(),
        };
        let message = under_context(self.context, &intrinsic);
        let message = match self.at {
            Some(Prov::Resolved(pair)) => format!(
                "{}:{}:{}: {message}",
                pair.1.script, pair.1.line, pair.1.col
            ),
            _ => message,
        };
        let mut notes = self
            .hints
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if self.unwound.discarded > 0 {
            notes.push(format!(
                "{} bytes written to stdout before the failure were discarded with the value; run the command without `let` to see them, or use `audit` to keep its output",
                self.unwound.discarded,
            ));
        }
        if self.unwound.abandoned_tail && !self.spawn && self.hints.is_empty() {
            notes.push(ABANDONED_TAIL_HINT.into());
        }
        (message, notes)
    }

    fn draw(&self, db: &SourceDb, message: &str, notes: &[String]) -> String {
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
            hint: (!notes.is_empty()).then(|| notes.join("\n\n")),
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
    fn compact(&self, message: &str, notes: &[String]) -> String {
        let (red, cyan, reset) = palette();
        if let Some(rejection) = self.rejection {
            let mut out = match self.context {
                Some(context) => format!("{context}: {}", rejection.render()),
                None => rejection.render(),
            };
            for hint in notes {
                let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
            }
            return out;
        }
        let mut out = format!("{red}error{reset}: {message}");
        if let Some(code) = self.shown {
            let _ = write!(out, " (exit status {code})");
        }
        out.push('\n');
        for hint in notes {
            let _ = writeln!(out, "{cyan}hint{reset}: {hint}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::CompileError;
    use crate::shell::runtime::End;
    use crate::source::{Source, SourceFile, SourceOrigin};
    use crate::syntax::parser::{ParseError, ParseErrorKind};

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
        let compile = CompileError::Parse(parse);
        let err = Error::new(Reason::Compile(Box::new(compile.failure("plugin.ral"))))
            .with_rejection(compile.reject(Source::from_text(file_origin("plugin.ral"), "let [x")))
            .discarded(412)
            .abandoning_tail()
            .with_context("use");
        let (db, root) = db_with("main.ral", "load-plugin plugin");
        let out = err.render(&db, Some(root), &ExitHints::default());
        assert!(out.contains("P0001") && out.contains("plugin.ral"), "{out}");
        assert_eq!(err.describe(), "loaded source \"plugin.ral\" did not parse");
        assert_eq!(err.failure_code(), NonZeroU8::MIN);
        assert_eq!(err.rejection().unwrap().status.get(), 2);
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
        let mut err = Error::new(Reason::raised("boom")).with_witness(witness);
        err.prov = span
            .filter(|sp| db.get(sp.file).is_some())
            .map(|span| Prov::Live {
                name: "fail".into(),
                span,
            });
        if let Some(hint) = hint {
            err = err.with_hint(hint);
        }
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
        let error = Error::new(Reason::raised("boom"))
            .discarded(200)
            .discarded(212)
            .abandoning_tail();
        let record = error.fact(&shell);
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
            .with_context("inner")
            .with_context("outer");
        let rendered = decorated.render(shell.sources(), None, shell.exit_hints());
        assert!(rendered.contains("outer: inner: boom"), "{rendered}");
        assert!(
            rendered.find("specific advice").unwrap() < rendered.find("412 bytes written").unwrap()
        );
        assert!(!rendered.contains("later steps"), "{rendered}");
        assert_eq!(decorated.fact(&shell), record);
        assert_eq!(decorated.discarded(u32::MAX).unwound().discarded, u32::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_carries_its_advice() {
        let reason = Reason::process("sh", Failure::Signaled(libc::SIGSEGV));
        assert_eq!(
            advice(&reason, &ExitHints::default()),
            ["the process crashed with a segmentation fault"]
        );
    }

    #[test]
    fn ordered_notes_stay_separate_until_layout() {
        let diagnosis = Diagnosis {
            message: "failure".into(),
            at: None,
            witness: None,
            rejection: None,
            hints: vec![
                "site one".into(),
                "site two".into(),
                "intrinsic one".into(),
                "intrinsic two".into(),
            ],
            shown: None,
            context: None,
            unwound: Unwound {
                abandoned_tail: false,
                discarded: 3,
            },
            spawn: false,
        };
        let (_, notes) = diagnosis.compose(None);
        assert_eq!(
            &notes[..4],
            ["site one", "site two", "intrinsic one", "intrinsic two"]
        );
        assert!(notes[4].starts_with("3 bytes written"));
        let rendered = crate::io::ansi::strip(&diagnosis.compact("failure", &notes));
        assert_eq!(rendered.matches("hint:").count(), 5);
    }

    #[test]
    fn portable_advice_is_rendered_not_stored() {
        let error = Error::new(Reason::Evaluation(Box::new(
            crate::fact::EvalFailure::MissingBinding("STATUS".into()),
        )))
        .with_hint("contextual advice");
        assert_eq!(error.hints().len(), 1);
        let output = error.plain();
        assert!(
            output.find("contextual advice").unwrap()
                < output.find("there is no status register").unwrap()
        );
        assert_eq!(error.hints().len(), 1);
    }

    #[test]
    fn stored_hints_come_first_and_the_records_advice_last() {
        let exit_hints = ExitHints::from_text("sh 3 the table's entry");
        let error = Error::new(Reason::process("sh", Failure::Exited(3)))
            .with_hint("the site's hint")
            .with_hint("a second site");
        let out = crate::io::ansi::strip(&error.compact(&exit_hints));
        let at = |needle| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {out}"))
        };
        assert!(at("the site's hint") < at("a second site"), "{out}");
        assert!(at("a second site") < at("the table's entry"), "{out}");
    }
    #[test]
    fn witness_line_is_resolved_only_for_source_aware_reports_before_context() {
        use crate::data::datum::Datum as _;
        use crate::fact::{
            AdmissionCause, AdmissionFailure, AdmissionUses, Expected, Found, InvalidCause,
            InvalidFailure, ValueShape,
        };
        let (db, file) = db_with("admission.ral", "bind\nuse\n");
        let reason = Reason::Invalid(Box::new(InvalidFailure::new(
            "ask".into(),
            InvalidCause::Admission(AdmissionFailure {
                pointer: String::new(),
                cause: AdmissionCause::Uses(AdmissionUses {
                    found: Found::Map,
                    expected: Expected::Shape(ValueShape::Variant),
                }),
            }),
        )));
        let intrinsic = reason.describe().into_owned();
        let mut error = Error::new(reason.clone())
            .with_witness(Some(Span::new(file, 5, 8)))
            .with_context("context");
        error.prov = Some(Prov::Live {
            name: "ask".into(),
            span: Span::new(file, 0, 4),
        });
        for compact in [None, Some(file)] {
            let rendered =
                crate::io::ansi::strip(&error.render(&db, compact, &ExitHints::default()));
            assert!(
                rendered.contains(&format!("context: {intrinsic}, line 2")),
                "{rendered}"
            );
            assert_eq!(rendered.matches(", line 2").count(), 1, "{rendered}");
            assert!(!rendered.contains(", line 1"), "{rendered}");
            assert_eq!(
                rendered.contains("the use this script makes of it"),
                compact.is_none(),
                "{rendered}"
            );
            let end = End::Err(error.clone());
            assert_eq!(
                end.render(&db, compact, &ExitHints::default()),
                error.render(&db, compact, &ExitHints::default())
            );
        }
        assert_eq!(reason.describe(), intrinsic);
        assert_eq!(error.describe(), intrinsic);
        assert!(!error.plain().contains(", line "));
        assert!(!error.compact(&ExitHints::default()).contains(", line "));
        assert!(
            !error
                .render_spanless(&ExitHints::default())
                .contains(", line ")
        );
        let decoded = Reason::decode(&reason.clone().encode()).unwrap();
        assert_eq!(decoded, reason);
        assert_eq!(decoded.describe(), intrinsic);
        error.prov = Some(Prov::Resolved(Box::new((
            "ask".into(),
            db.site(Span::new(file, 0, 4)).unwrap(),
        ))));
        let resolved =
            crate::io::ansi::strip(&error.render(&db, Some(file), &ExitHints::default()));
        assert!(
            resolved.contains(&format!("admission.ral:1:1: context: {intrinsic}, line 2")),
            "{resolved}"
        );
        let record = error.fact(&crate::test_helper::core_shell());
        let decoded = crate::fact::ErrorFact::decode(&record.clone().encode()).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(record.describe(), intrinsic);
    }

    #[test]
    fn witness_line_never_falls_back_to_the_primary_site_or_another_database() {
        let (mut db, file) = db_with("primary.ral", "first\nsecond\n");
        let witness_file = db.register(Source::from_text(
            file_origin("witness.ral"),
            "first\nsecond\nthird\n",
        ));
        let mut error =
            Error::new(Reason::raised("boom")).with_witness(Some(Span::new(witness_file, 13, 18)));
        error.prov = Some(Prov::Live {
            name: "fail".into(),
            span: Span::new(file, 6, 12),
        });
        let rendered = crate::io::ansi::strip(&error.render(&db, None, &ExitHints::default()));
        assert!(rendered.contains("boom, line 3"), "{rendered}");
        assert!(
            !rendered.contains("the use this script makes of it"),
            "{rendered}"
        );
        assert!(
            !error
                .render(&SourceDb::default(), None, &ExitHints::default())
                .contains(", line ")
        );
        let absent = error.with_witness(Some(Span::new(FileId::DUMMY, 0, 1)));
        for compact in [None, Some(file)] {
            assert!(
                !absent
                    .render(&db, compact, &ExitHints::default())
                    .contains(", line ")
            );
        }
        assert!(
            !absent
                .render(&SourceDb::default(), None, &ExitHints::default())
                .contains(", line ")
        );
    }

    #[test]
    fn general_advice_follows_context_but_denial_advice_still_precedes_it() {
        use crate::fact::{Denial, EvalFailure, SingleOrNested, Withholding};
        let error = Error::new(Reason::Evaluation(Box::new(EvalFailure::MissingBinding(
            "x-y".into(),
        ))))
        .with_hint("a hyphen boundary specific to this scope");
        assert_eq!(error.hints().len(), 1);
        let rendered = error.plain();
        assert!(
            rendered.find("a hyphen boundary").unwrap() < rendered.find("check spelling").unwrap()
        );
        let reason = Reason::Denied(Box::new(Denial::exec_run(
            "git".into(),
            Withholding::MissingAllowance(SingleOrNested::Nested),
        )));
        let intrinsic = reason.advice()[0].clone();
        let error = Error::new(reason).with_hint("this invocation's search context");
        let rendered = error.plain();
        assert!(
            rendered.find(&intrinsic).unwrap()
                < rendered.find("this invocation's search context").unwrap()
        );
        assert_eq!(error.hints().len(), 1);
    }
}

#[cfg(test)]
mod report_context_tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::fact::{
        AgentFailure, AgentFailureReplyFailed, ContextFailure, ContextFailureCurrentlyWritten,
        Denial, ExarchFailure, HelperFailure, HostFailure, IoCause, IoContext, IoFailure, IoKind,
        IoLeaf, IoOperation, IoSubject, OsFailure, PathIoContext, SingleOrNested, Withholding,
    };

    fn context_reason(cause: ContextFailure) -> Reason {
        Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Context(cause))))
    }
    fn check(reason: &Reason, context: Option<&str>, expected: &[&str]) {
        let decoded = Reason::decode(&reason.clone().encode()).unwrap();
        assert_eq!(&decoded, reason);
        for reason in [reason, &decoded] {
            let description = reason.describe().into_owned();
            let mut error = Error::new(reason.clone());
            if let Some(context) = context {
                error = error.with_context(context);
            }
            assert_eq!(error.describe(), description);
            assert_eq!(error.hints(), []);
            assert_eq!(context_advice(&error.reason, error.context()), expected);
            let error = error.with_hint("stored-first");
            let rendered = error.plain();
            let hints: Vec<_> = rendered
                .lines()
                .filter_map(|line| line.strip_prefix("hint: "))
                .collect();
            let intrinsic = reason.advice();
            let mut wanted = vec!["stored-first"];
            wanted.extend_from_slice(expected);
            wanted.extend(intrinsic.iter().map(String::as_str));
            assert_eq!(hints, wanted);
            assert_eq!(
                error
                    .hints()
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<Vec<&str>>(),
                ["stored-first"]
            );
            let _ = error.render(&SourceDb::default(), None, &ExitHints::default());
        }
    }

    #[test]
    fn direct_context_guidance_requires_exact_operation_and_cause() {
        let current = ContextFailure::CurrentlyWritten(ContextFailureCurrentlyWritten {
            turn: u64::MAX.into(),
        });
        for (cause, operation, hint) in [
            (
                ContextFailure::EmptySelection,
                "exarch-transcript `read",
                "`!{range a b}` builds a run of ids",
            ),
            (
                ContextFailure::EmptySelection,
                "exarch-transcript `grep",
                "omit turns to search the whole transcript",
            ),
            (
                current.clone(),
                "exarch-transcript `read",
                "it is the one turn the transcript cannot read back yet",
            ),
            (
                current.clone(),
                "exarch-transcript `grep",
                "it is the one turn the transcript cannot read back yet",
            ),
            (
                current,
                "exarch-context `evict",
                "an eviction keeps the work in hand",
            ),
        ] {
            let reason = context_reason(cause.clone());
            check(&reason, Some(operation), &[hint]);
            for context in [
                None,
                Some("unrelated"),
                Some(&format!("{operation}: extra")),
                Some(&format!("{operation}`")),
                Some(&format!("prefix: {operation}")),
            ] {
                check(&reason, context, &[]);
            }
            let nested = Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Agent(
                AgentFailure::ReplyFailed(AgentFailureReplyFailed {
                    name: "a".into(),
                    cause: HelperFailure::Context(cause),
                }),
            ))));
            check(&nested, Some(operation), &[]);
            check(
                &context_reason(ContextFailure::SteeringWithoutTurn),
                Some(operation),
                &[],
            );
        }
        check(
            &context_reason(ContextFailure::EmptySelection),
            Some("exarch-context `evict"),
            &[],
        );
    }

    #[test]
    fn contextual_missing_paths_never_replace_generic_io_or_infer_from_near_matches() {
        for (operation, tools) in [
            (
                IoOperation::Open,
                ["view-text", "view-hash", "edit-hash", "edit-replace"],
            ),
            (
                IoOperation::Walk,
                ["grep-files", "grep-hidden", "explore-dir", "find-files"],
            ),
        ] {
            let io = IoFailure {
                operation,
                subject: IoSubject::Path("nope".into()),
                cause: IoCause::new(IoLeaf::Os(OsFailure {
                    kind: IoKind::NotFound,
                    raw: None,
                    detail: None,
                })),
            };
            let reason = Reason::Io(Box::new(io.clone()));
            assert_eq!(
                reason.describe(),
                format!(
                    "could not {} \"nope\": no such file or directory",
                    operation.describe()
                )
            );
            for tool in tools {
                let hint = if operation == IoOperation::Open {
                    format!(
                        "nope does not exist; {tool} never creates a file; `to-string BODY > path` writes a new one"
                    )
                } else {
                    "nope does not exist; the path is a file or directory, relative to the cwd or absolute".into()
                };
                check(&reason, Some(tool), &[&hint]);
                for context in [
                    None,
                    Some("other"),
                    Some(&format!("{tool}: extra")),
                    Some(&format!("prefix: {tool}")),
                ] {
                    check(&reason, context, &[]);
                }
                for changed in [
                    IoFailure {
                        operation: IoOperation::Read,
                        ..io.clone()
                    },
                    IoFailure {
                        subject: IoSubject::HostFile("nope".into()),
                        ..io.clone()
                    },
                    IoFailure {
                        cause: IoCause::new(IoLeaf::Os(OsFailure {
                            kind: IoKind::PermissionDenied,
                            raw: None,
                            detail: None,
                        })),
                        ..io.clone()
                    },
                    IoFailure {
                        cause: IoCause {
                            context: vec![IoContext::Path(PathIoContext::UnsafeScopedInheritance)],
                            leaf: io.cause.leaf.clone(),
                        },
                        ..io.clone()
                    },
                ] {
                    check(&Reason::Io(Box::new(changed)), Some(tool), &[]);
                }
            }
            for wrong in if operation == IoOperation::Open {
                ["grep-files", "find-files"]
            } else {
                ["view-text", "edit-hash"]
            } {
                check(&reason, Some(wrong), &[]);
            }
        }
    }

    #[test]
    fn denial_keeps_intrinsic_before_stored_and_never_gets_missing_file_guidance() {
        let reason = Reason::Denied(Box::new(Denial::exec_run(
            "nope".into(),
            Withholding::MissingAllowance(SingleOrNested::Nested),
        )));
        let intrinsic = reason.advice();
        let error = Error::new(reason)
            .with_context("view-text")
            .with_hint("stored-last");
        assert_eq!(
            context_advice(&error.reason, error.context()),
            Vec::<String>::new()
        );
        let plain = error.plain();
        for hint in intrinsic {
            assert!(plain.find(&hint).unwrap() < plain.find("stored-last").unwrap());
        }
        assert!(!plain.contains("never creates a file"));
    }
}
