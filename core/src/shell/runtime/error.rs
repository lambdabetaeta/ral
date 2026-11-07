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

/// The one hint the record determines: signal advice for `signaled`, the
/// session's exit-hints entry for `exited`.  Recomputed at each render.
fn advice<'a>(reason: &'a Reason, exit_hints: &ExitHints) -> Option<Cow<'a, str>> {
    let Reason::Process(process) = reason else {
        return None;
    };
    match process.failure {
        #[cfg(unix)]
        Failure::Signaled(libc::SIGKILL) => Some(
            "the process was killed with SIGKILL; the kernel or another process may have terminated it".into(),
        ),
        #[cfg(unix)]
        Failure::Signaled(libc::SIGSEGV) => {
            Some("the process crashed with a segmentation fault".into())
        }
        Failure::Signaled(n) => Some(
            format!(
                "the process terminated from {}",
                crate::process::Signal::new(n)
            )
            .into(),
        ),
        Failure::Exited(n) => exit_hints.lookup(&process.program, n).map(Cow::Owned),
        Failure::NotFound | Failure::NotRunnable(_) => None,
    }
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
        let (message, hint) = diagnosis.compose();
        diagnosis.compact(&message, hint.as_deref())
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
        let hints = self
            .hints()
            .iter()
            .map(|hint| Cow::Borrowed(&**hint))
            .chain(advice(&self.reason, exit_hints))
            .collect();
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
        let message = under_context(self.context, &self.message);
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
        let err = Error::new(compile.to_string())
            .with_rejection(compile.reject(Source::from_text(file_origin("plugin.ral"), "let [x")))
            .discarded(412)
            .abandoning_tail()
            .with_context("use");
        let (db, root) = db_with("main.ral", "load-plugin plugin");
        let out = err.render(&db, Some(root), &ExitHints::default());
        assert!(out.contains("P0001") && out.contains("plugin.ral"), "{out}");
        assert_eq!(err.describe(), "parse error: expected ]");
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
        let mut err = Error::new("boom").with_witness(witness);
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
            .with_context("inner")
            .with_context("outer");
        let rendered = decorated.render(shell.sources(), None, shell.exit_hints());
        assert!(rendered.contains("outer: inner: boom"), "{rendered}");
        assert!(
            rendered.find("specific advice").unwrap() < rendered.find("412 bytes written").unwrap()
        );
        assert!(!rendered.contains("later steps"), "{rendered}");
        assert_eq!(decorated.record(&shell), record);
        assert_eq!(decorated.discarded(u32::MAX).unwound.discarded, u32::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_carries_its_advice() {
        let reason = Reason::process("sh", Failure::Signaled(libc::SIGSEGV));
        assert_eq!(
            advice(&reason, &ExitHints::default()).as_deref(),
            Some("the process crashed with a segmentation fault")
        );
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
}
