//! The run's projection onto the wire: the one lossy step between the
//! engine's [`RunReport`] and the protocol's [`Report`].
//!
//! Rendering belongs here because this is the last point at which the
//! engine's shell is in hand; the host receives the full string (prefix,
//! status, hint, caret) and only has to print it.

use crate::first_order::datum::Datum;
use crate::first_order::{FOValue, NotData, Opaque};
use crate::protocol::{Ending, FailureStatus, Report};
use crate::run::RunReport;
#[cfg(any(unix, test))]
use crate::run::StaticDiagnostics;
use crate::types::{Error, Shell};

/// A settled result that cannot cross, said with what the author likely
/// meant.
fn unreturnable(message: String, hint: &str, shell: &Shell) -> Ending {
    let mut error = Error::new(message);
    error.hint = Some(hint.into());
    Ending::Unreturnable {
        rendered: error.compact(),
        record: record_of(&error, shell),
    }
}

/// The record `try` hands its handler for `error`.
fn record_of(error: &Error, shell: &Shell) -> FOValue {
    error.record(shell).encode()
}

fn not_data(NotData { leaf, nested }: NotData, shell: &Shell) -> Ending {
    let (verb, hint) = match (leaf, nested) {
        (Opaque::Handle, false) => (
            "is",
            "did you mean to keep it? bind it (`let h = defer { … }`) and `await $h` \
             when you need its value",
        ),
        (Opaque::Block, false) => ("is", "did you mean to run it? force it with `!{ … }`"),
        (Opaque::Function, false) => ("is", "did you mean to apply it? pass it its arguments"),
        (Opaque::Handle, true) => (
            "holds",
            "bind the handle with `let h = …`, and return only data",
        ),
        (Opaque::Block | Opaque::Function, true) => (
            "holds",
            "return only data: force a block with `!{ … }`, or bind it with `let`",
        ),
    };
    unreturnable(
        format!("the result {verb} {leaf}, and a run can return only data"),
        hint,
        shell,
    )
}

/// Project an engine [`Ending`](crate::run::Ending) onto the wire, rendering
/// a caught runtime error against `shell` — the one lossy step between the
/// engine and the protocol: the live `Error` renders to the string the host
/// prints verbatim, and to its record.
fn render_ending(ending: crate::run::Ending, shell: &Shell) -> Ending {
    use crate::run::Ending as Raw;
    match ending {
        // A top-level result is not an `Observation` — it has no placeholder
        // vocabulary of its own — so a value the wire cannot carry is
        // reported, never silently dropped.
        Raw::Settled { value, status } => match FOValue::try_from(&value) {
            Ok(fo) => Ending::Settled {
                value: fo,
                status: status.clamp(0, 255),
            },
            Err(found) => not_data(found, shell),
        },
        Raw::Raised { error, compact } => {
            let (rendered, record, status) = render_raise(&error, compact, shell);
            Ending::Raised {
                rendered,
                record,
                command_exit: error.status.exited().is_some(),
                single_command: compact.is_some(),
                status,
            }
        }
        Raw::Walled { error, compact } => {
            let (rendered, record, status) = render_raise(&error, compact, shell);
            Ending::Walled {
                rendered,
                record,
                status,
            }
        }
        Raw::Exited(code) => Ending::Exited(code.clamp(0, 255)),
    }
}

/// What `Raised` and `Walled` share: the rendered error, its record and its
/// status.
fn render_raise(
    error: &Error,
    compact: Option<crate::source::FileId>,
    shell: &Shell,
) -> (String, FOValue, FailureStatus) {
    (
        error.render(shell.sources(), compact),
        record_of(error, shell),
        FailureStatus::from(error.code()),
    )
}

impl RunReport {
    /// Project into the protocol [`Report`] — the one lossy step between the
    /// engine and the protocol: the live `Value` becomes an [`FOValue`], and rich
    /// diagnostics render to strings.
    pub(crate) fn into_report(self, shell: &Shell) -> Report {
        match self {
            // Not the shell's sources: a static failure carries the text its
            // carets point into, so nothing about it was ever registered.
            Self::Static { diagnostics } => {
                let (rendered, status) = diagnostics.render();
                Report::Static { rendered, status }
            }
            Self::Ran {
                ending,
                captured,
                trail,
            } => Report::Ran {
                ending: render_ending(ending, shell),
                captured,
                trail: trail.into_iter().map(Datum::encode).collect(),
            },
        }
    }
}

impl Report {
    /// A refusal the engine raises on its own behalf — a panicked worker, a
    /// busy engine — rendered through the same door as a run's own static
    /// failure, so a host never has to know which of the two it is printing.
    #[cfg(unix)]
    pub(crate) fn host_fault(message: impl Into<String>) -> Self {
        let (rendered, status) = StaticDiagnostics::Host(Error::new(message)).render();
        Self::Static { rendered, status }
    }
}

// ── Runtime-error protocol tests ───────────────────────────────────────
//
// Rendering at the protocol is what gives every front-end, the REPL included,
// batch-host parity: it prints the string verbatim and renders nothing itself.
#[cfg(test)]
mod runtime_error_seam_tests {
    use super::*;

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let shell = crate::test_helper::core_shell();
        let err = Error::raised("boom", 3).with_hint("try harder");
        // A single command whose error carries no span: the compact one-liner.
        let (rendered, record, _) = render_raise(&err, Some(crate::source::FileId(0)), &shell);
        assert_eq!(
            record.field("message"),
            Some(&FOValue::String {
                value: "boom".into()
            }),
            "the record carries the bare message: {record:?}"
        );
        assert!(
            rendered.contains("error"),
            "error prefix missing: {rendered:?}"
        );
        assert!(rendered.contains("boom"), "message missing: {rendered:?}");
        assert!(
            rendered.contains("exit status 3"),
            "exit status missing: {rendered:?}"
        );
        assert!(
            rendered.contains("try harder"),
            "hint missing: {rendered:?}"
        );
        assert!(
            rendered.ends_with('\n'),
            "the protocol must supply the trailing newline the host prints verbatim: {rendered:?}"
        );
    }
}

// ── The static seam ──────────────────────────────────────────────────
//
// The counterpart law for a run that never reached evaluation: the caret, the
// code and the hint survive the projection, and the registry never learns of
// text no live span can index.
#[cfg(test)]
mod static_diagnostic_seam_tests {
    use super::*;
    use crate::protocol::Run;

    /// The wire report for `src`, and how many registry ids the run minted.
    fn project(src: &str) -> (String, i32, u32) {
        let mut shell = crate::test_helper::core_shell();
        let before = shell.sources().next_id().0;
        let report = shell.run(Run::captured(src, "<test>"));
        assert!(
            matches!(report, RunReport::Static { .. }),
            "{src:?} must fail before evaluation"
        );
        let minted = shell.sources().next_id().0 - before;
        let Report::Static { rendered, status } = report.into_report(&shell) else {
            panic!("a static run must project to Report::Static");
        };
        (rendered, status, minted)
    }

    #[test]
    fn a_parse_failure_projects_to_a_caret_report() {
        let (rendered, status, _) = project("let = ");
        assert_eq!(status, 2, "a parse failure exits 2: {rendered:?}");
        assert!(
            rendered.contains("[P0001]"),
            "the code must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("<test>:1:5"),
            "the resolved position must survive: {rendered:?}"
        );
        assert!(
            rendered.contains('╰'),
            "the caret must survive: {rendered:?}"
        );
    }

    /// `code()`, `render_label()` and `hint()` are exactly what `Display` drops.
    #[test]
    fn a_type_failure_projects_with_its_code_label_and_hint() {
        let (rendered, status, _) = project("$[1 + true]");
        assert_eq!(status, 1, "a type failure exits 1: {rendered:?}");
        assert!(
            rendered.contains("[T0010]"),
            "the code must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("Integer doesn't match Bool"),
            "the under-caret label must survive: {rendered:?}"
        );
        assert!(
            rendered.contains("Help:"),
            "the hint must survive: {rendered:?}"
        );
        assert!(
            !rendered.contains("@0.."),
            "raw byte offsets must not reach a host: {rendered:?}"
        );
    }

    /// A failed compile leaves no live span, so its text has no slot.
    #[test]
    fn a_static_failure_registers_no_source() {
        for src in ["let = ", "$[1 + true]"] {
            let (_, _, minted) = project(src);
            assert_eq!(minted, 0, "{src:?} must not grow the registry");
        }
    }

    /// The `Host` arm is spanless, so it renders as the one-liner.
    #[test]
    fn a_host_fault_renders_without_a_caret() {
        let (rendered, status) =
            StaticDiagnostics::Host(crate::types::Error::new("hook 'x' is not registered"))
                .render();
        assert_eq!(status, 1);
        assert!(rendered.contains("hook 'x' is not registered"));
        assert!(!rendered.contains('╰'), "no span, no caret: {rendered:?}");
        assert!(rendered.ends_with('\n'));
    }
}
