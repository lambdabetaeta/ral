//! The run's projection onto the wire: the one lossy step between the
//! engine's [`RunReport`] and the protocol's [`Report`].
//!
//! Rendering belongs here because this is the last point at which the
//! engine's shell is in hand; the host receives the full string (prefix,
//! status, hint, caret) and only has to print it.

use crate::data::datum::Datum;
use crate::data::{FOValue, NotData, Opaque};
use crate::fact::Ending;
use crate::protocol::{Ran, Report};
use crate::shell::run::{RunReport, spanless};
use crate::shell::runtime::{End, Error, Shell};
use std::convert::identity;

fn not_data(NotData { leaf, nested }: NotData) -> Error {
    let (verb, hint) = match (leaf, nested) {
        (Opaque::Handle, false) => (
            "is",
            "did you mean to keep it? bind it (`let h = spawn \"title\" { … }`) and \
             `await $h` when you need its value",
        ),
        (Opaque::Block | Opaque::Function, false) => (
            "is",
            "did you mean to run it? force it with `!{ … }`, or pass it its arguments",
        ),
        (Opaque::Handle, true) => (
            "holds",
            "bind the handle with `let h = …`, and return only data",
        ),
        (Opaque::Block | Opaque::Function, true) => (
            "holds",
            "return only data: force a block with `!{ … }`, or bind it with `let`",
        ),
    };
    Error::new(format!(
        "the result {verb} {leaf}, and a run can return only data"
    ))
    .with_hint(hint)
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
                compact,
                captured,
                trail,
            } => {
                let ending = match ending {
                    End::Ok(value) => FOValue::try_from(&value).map_err(not_data).into(),
                    End::Err(error) => Ending::Err(error),
                    End::Exited(code) => Ending::Exited(code),
                    End::Stopped(stop) => Ending::Stopped(stop),
                    End::Panic(text) => Ending::Panic(text),
                };
                let rendered = ending.render(shell.sources(), compact);
                let ending = ending.map(identity, |error| error.record(shell), |stop| stop.cause);
                Report::Ran(Ran {
                    ending,
                    rendered,
                    single_command: compact.is_some(),
                    captured,
                    trail: trail.into_iter().map(Datum::encode).collect(),
                })
            }
        }
    }
}

impl Report {
    /// A refusal before the run reached evaluation (a busy engine, a dispatch
    /// that could not be sent), rendered as the same one-liner a run's own
    /// host-level static failure is, so a host never has to know which of the
    /// two it is printing.
    pub(crate) fn refused(message: impl Into<String>) -> Self {
        let (rendered, status) = spanless(&Error::new(message));
        Self::Static { rendered, status }
    }

    /// A fault outside the run's own report (a panicked engine worker, a
    /// report the frame fuse would not pass): the run may have run, in part or
    /// whole, so it is no refusal. Rendered as the same one-liner a refusal is.
    /// Only the wire engine, a unix process, has an outside to fault in.
    #[cfg(unix)]
    pub(crate) fn faulted(message: impl Into<String>) -> Self {
        let (rendered, status) = spanless(&Error::new(message));
        Self::Faulted { rendered, status }
    }
}

// ── Runtime-error protocol tests ───────────────────────────────────────
//
// Rendering at the protocol is what gives every front-end, the REPL included,
// batch-host parity: it prints the string verbatim and renders nothing itself.
#[cfg(test)]
mod runtime_error_seam_tests {
    use super::*;
    use std::num::NonZeroU8;

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let shell = crate::test_helper::core_shell();
        let err = Error::raised("boom", NonZeroU8::new(3).unwrap()).with_hint("try harder");
        // A single command whose error carries no span: the compact one-liner.
        let Report::Ran(Ran {
            rendered,
            ending: Ending::Err(record),
            single_command,
            ..
        }) = (RunReport::Ran {
            ending: End::Err(err),
            compact: Some(crate::source::FileId(0)),
            captured: None,
            trail: Vec::new(),
        })
        .into_report(&shell)
        else {
            panic!("expected failed run")
        };
        assert!(single_command);
        assert_eq!(
            record.message(),
            "boom",
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
    use crate::shell::run::StaticDiagnostics;
    use crate::source::SourceOrigin;
    use std::num::NonZeroU8;

    /// The wire report for `src`, and how many registry ids the run minted.
    fn project(src: &str) -> (String, u8, u32) {
        let mut shell = crate::test_helper::core_shell();
        let before = shell.sources().next_id().0;
        let report = shell.run(Run::captured(src, SourceOrigin::named("<test>")));
        assert!(
            matches!(report, RunReport::Static { .. }),
            "{src:?} must fail before evaluation"
        );
        let minted = shell.sources().next_id().0 - before;
        let Report::Static { rendered, status } = report.into_report(&shell) else {
            panic!("a static run must project to Report::Static");
        };
        (rendered, status.get(), minted)
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
        let (rendered, status) = StaticDiagnostics::Host(crate::shell::runtime::Error::new(
            "hook 'x' is not registered",
        ))
        .render();
        assert_eq!(status, NonZeroU8::MIN);
        assert!(rendered.contains("hook 'x' is not registered"));
        assert!(!rendered.contains('╰'), "no span, no caret: {rendered:?}");
        assert!(rendered.ends_with('\n'));
    }

    /// A fault is a `Faulted`, not the `Static` a refusal is, yet the same
    /// one-liner.
    #[cfg(unix)]
    #[test]
    fn a_fault_is_a_faulted_one_liner() {
        let Report::Faulted { rendered, status } = Report::faulted("worker panicked") else {
            panic!("a fault must be Report::Faulted");
        };
        assert_eq!(status.get(), 1);
        assert!(rendered.contains("worker panicked") && !rendered.contains('╰'));
    }

    #[test]
    fn a_panicked_run_projects_to_a_panic_ending() {
        let shell = crate::test_helper::core_shell();
        let report = RunReport::Ran {
            ending: End::Panic("boom".into()),
            compact: None,
            captured: None,
            trail: Vec::new(),
        }
        .into_report(&shell);
        assert_eq!(report.code(), crate::fact::PANIC_CODE);
        let Report::Ran(Ran {
            ending: Ending::Panic(text),
            rendered,
            ..
        }) = report
        else {
            panic!("expected panic ending");
        };
        assert_eq!(text, "boom");
        assert!(rendered.contains("run panicked: boom"), "{rendered:?}");
    }

    #[test]
    fn a_non_data_result_projects_to_a_failure() {
        let mut shell = crate::test_helper::core_shell();
        let report = shell
            .run(Run::captured(
                "spawn { return 1 }",
                SourceOrigin::named("<test>"),
            ))
            .into_report(&shell);
        assert_eq!(report.code(), 1);
        let Report::Ran(Ran {
            ending: Ending::Err(_),
            rendered,
            ..
        }) = report
        else {
            panic!("expected failed projection");
        };
        assert!(rendered.contains("`await $h`"), "{rendered:?}");
    }

    #[test]
    fn ok_and_exit_render_nothing() {
        let mut shell = crate::test_helper::core_shell();
        for src in ["return 1", "exit 3"] {
            let report = shell
                .run(Run::captured(src, SourceOrigin::named("<test>")))
                .into_report(&shell);
            let Report::Ran(Ran { rendered, .. }) = report else {
                panic!("expected run")
            };
            assert!(rendered.is_empty(), "{src}: {rendered:?}");
        }
    }
}
