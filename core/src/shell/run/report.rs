//! The run's projection onto the wire: the one lossy step between the
//! engine's [`RunReport`] and the protocol's [`Report`].
//!
//! Rendering belongs here because this is the last point at which the
//! engine's shell is in hand; the host receives the full string (prefix,
//! status, hint, caret) and only has to print it.

use crate::data::datum::Datum;
use crate::data::{FOValue, NotData};
use crate::fact::Ending;
use crate::fact::{InvalidCause, Reason};
use crate::protocol::{Ran, Report};
use crate::shell::run::{RunReport, spanless};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{End, Error, Shell};
use std::convert::identity;

fn not_data(error: NotData) -> Error {
    invalid("run", InvalidCause::NonData(error.into()))
}

/// A report lost after evaluation may already have started.
#[cfg(unix)]
pub(crate) enum ReportFault {
    WorkerPanicked,
    Unencodable(crate::fact::FrameEncodeFailure),
}

impl RunReport {
    /// Project into the protocol [`Report`] — the one lossy step between the
    /// engine and the protocol: the live `Value` becomes an [`FOValue`], and rich
    /// diagnostics render to strings.
    pub(crate) fn into_report(self, shell: &Shell) -> Report {
        match self {
            Self::Static { diagnostics, trail } => {
                let (rendered, status) = diagnostics.render(shell.exit_hints());
                Report::Static {
                    rendered,
                    status,
                    trail: trail.into_iter().map(Datum::encode).collect(),
                }
            }
            Self::Checked { trail } => Report::Checked {
                trail: trail.into_iter().map(Datum::encode).collect(),
            },
            Self::Ran {
                ending,
                base,
                compact,
                captured,
                trail,
            } => {
                let ending = match ending {
                    End::Ok(value) => FOValue::try_from(&value)
                        .map_err(|error| {
                            let mut error = not_data(error);
                            error.prov = Some(base);
                            error
                        })
                        .into(),
                    End::Err(error) => Ending::Err(error),
                    End::Exited(code) => Ending::Exited(code),
                    End::Stopped(stop) => Ending::Stopped(stop),
                    End::Panic(text) => Ending::Panic(text),
                };
                let rendered = ending.render(shell.sources(), compact, shell.exit_hints());
                let ending = ending.map(identity, |error| error.fact(shell), |stop| stop.cause);
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
    pub(crate) fn refused(reason: Reason) -> Self {
        let (rendered, status) = spanless(
            &Error::new(reason),
            &crate::shell::runtime::ExitHints::default(),
        );
        Self::Static {
            rendered,
            status,
            trail: Vec::new(),
        }
    }

    /// A fault outside the run's own report (a panicked engine worker, a
    /// report the frame fuse would not pass): the run may have run, in part or
    /// whole, so it is no refusal. Rendered as the same one-liner a refusal is.
    /// Only the wire engine, a unix process, has an outside to fault in.
    #[cfg(unix)]
    pub(crate) fn faulted(fault: ReportFault) -> Self {
        let message = match fault {
            ReportFault::WorkerPanicked => "engine: dispatch panicked in the engine worker".into(),
            ReportFault::Unencodable(cause) => format!(
                "engine: run report could not be encoded: {}",
                cause.describe()
            ),
        };
        let rendered = crate::diagnostic::Report {
            code: Some("R0001"),
            message,
            at: None,
            also: None,
            hint: None,
        }
        .plain();
        Self::Faulted {
            rendered,
            status: std::num::NonZeroU8::MIN,
        }
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
    fn a_wire_data_refusal_keeps_its_run_base_after_the_register_is_restored() {
        let mut shell = crate::test_helper::core_shell();
        let before = shell.local.audit.call_site().clone();
        let report = shell.run(crate::protocol::Run::captured(
            "return { return 1 }",
            crate::source::SourceOrigin::named("<data-boundary>"),
        ));
        assert_eq!(shell.local.audit.call_site(), &before);
        let Report::Ran(Ran {
            ending: Ending::Err(record),
            rendered,
            ..
        }) = report.into_report(&shell)
        else {
            panic!("a block cannot cross the data boundary")
        };
        assert_eq!(record.frame(), "<data-boundary>");
        assert_eq!(record.site().script.as_ref(), "<data-boundary>");
        assert_eq!((record.site().line, record.site().col), (1, 1));
        assert!(rendered.contains("<data-boundary>"), "{rendered}");
    }

    #[test]
    fn runtime_error_projects_to_a_full_diagnostic_string() {
        let shell = crate::test_helper::core_shell();
        let err = Error::new(Reason::raised("boom")).with_hint("try harder");
        // A single command whose error carries no span: the compact one-liner.
        let Report::Ran(Ran {
            rendered,
            ending: Ending::Err(record),
            single_command,
            ..
        }) = (RunReport::Ran {
            ending: End::Err(err),
            base: shell.local.audit.call_site().clone(),
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
            record.describe(),
            "boom",
            "the record carries the bare message: {record:?}"
        );
        assert!(
            rendered.contains("error"),
            "error prefix missing: {rendered:?}"
        );
        assert!(rendered.contains("boom"), "message missing: {rendered:?}");
        assert!(
            !rendered.contains("exit status"),
            "a raised reason has no redundant exit status: {rendered:?}"
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
// code and the hint survive the projection, and the root retains its registry id.
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
        let Report::Static {
            rendered, status, ..
        } = report.into_report(&shell)
        else {
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

    #[test]
    fn a_static_failure_registers_its_root_once() {
        for src in ["let = ", "$[1 + true]"] {
            let (_, _, minted) = project(src);
            assert_eq!(minted, 1, "{src:?} must retain its registered root");
        }
    }

    /// The `Host` arm is spanless, so it renders as the one-liner.
    #[test]
    fn a_host_fault_renders_without_a_caret() {
        let reason = Reason::State(Box::new(crate::fact::StateFailure::MissingHook(
            crate::fact::HookRef::Session("x".into()),
        )));
        let expected = reason.describe().into_owned();
        let (rendered, status) = StaticDiagnostics::Host(Error::new(reason))
            .render(&crate::shell::runtime::ExitHints::default());
        assert_eq!(status, NonZeroU8::MIN);
        assert!(rendered.contains(&expected));
        assert!(!rendered.contains('╰'), "no span, no caret: {rendered:?}");
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn a_panicked_run_projects_to_a_panic_ending() {
        let shell = crate::test_helper::core_shell();
        let report = RunReport::Ran {
            ending: End::Panic("boom".into()),
            base: shell.local.audit.call_site().clone(),
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
