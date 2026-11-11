//! The loading half of exarch's capability composition: `ral_core`'s profile
//! loader under the flag that named the file, and the cwd-relative path helper
//! `super::for_invocation` calls around it.

use ral_core::authority::grant::freeze::FreezeCtx;
use ral_core::authority::policy::{Fragment, GrantLayer};
use ral_core::error::Error;
use ral_core::fact::{
    EscapedLoad, ExarchFailure, HostFailure, PolicyFailure, PolicyFailureEscapedLoad, Reason,
};
use ral_core::shell::runtime::{Break, Escape, Mooring, Shell};
use std::path::{Path, PathBuf};

use ral_core::environ::EnvVars;
use ral_core::path;

/// Read a capabilities profile from `path` as a resolved [`GrantLayer`].
///
/// Sigils freeze against `ctx` here, so a bad `xdg:` fails at the profile that
/// names it even where a later `meet` would have discarded the grant, and an
/// `exit` inside the profile flattens into an error: a profile is
/// configuration, not control flow.
pub(super) fn load_capabilities_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<GrantLayer, Error> {
    load(flag, || {
        ral_core::shell::load::profile::load_capabilities_from_path(mooring, shell, path, ctx)
    })
}

/// [`load_capabilities_ral`] for an extension file, which only
/// [`GrantLayer::extend`] may consume.
pub(super) fn load_fragment_ral(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    flag: &str,
    ctx: &FreezeCtx,
) -> Result<Fragment, Error> {
    load(flag, || {
        ral_core::shell::load::profile::load_fragment_from_path(mooring, shell, path, ctx)
    })
}

/// `read`'s failure under `flag`: its words already name the file.
pub(super) fn load<T>(flag: &str, read: impl FnOnce() -> Result<T, Break>) -> Result<T, Error> {
    read().map_err(|e| match e {
        Break::Error(err) => err.with_context(flag),
        Break::Escape(escape) => Error::new(Reason::Host(Box::new(HostFailure::Exarch(
            ExarchFailure::Policy(PolicyFailure::EscapedLoad(PolicyFailureEscapedLoad {
                flag: flag.into(),
                cause: match escape {
                    Escape::Exit(code) => EscapedLoad::Exit(code),
                    Escape::Stopped(stop) => EscapedLoad::Stopped(stop.cause),
                },
            })),
        )))),
    })
}

/// Anchor `p`, a path the host was handed on the command line, at `cwd`
/// unless it is already absolute: see [`path::Resolver::anchor`].
pub(super) fn absolute_in(cwd: &path::CwdPath, p: &Path) -> PathBuf {
    path::Resolver::at(&EnvVars::new(), cwd)
        .anchor(p)
        .into_inner()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::fact::{CompilePhase, StateFailure};
    use ral_core::process::CancelCause;
    use ral_core::shell::runtime::Stop;
    use ral_core::source::SourceOrigin;

    #[test]
    fn load_keeps_an_existing_reason_and_report_facts() {
        let error = Error::new(Reason::State(Box::new(StateFailure::NoDesk)))
            .with_hint("situational first")
            .with_hint("situational second");
        let reason = error.reason.clone();
        let got = load::<()>("--restrict", || Err(Break::Error(error))).unwrap_err();
        assert_eq!(got.reason, reason);
        assert_eq!(got.context(), Some("--restrict"));
        assert_eq!(
            got.hints().iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            ["situational first", "situational second"]
        );
    }

    #[test]
    fn load_keeps_exit_bytes_and_each_stop_cause_distinct() {
        let exits = [0, 255]
            .into_iter()
            .map(|code| (Escape::Exit(code), EscapedLoad::Exit(code)));
        let stops = [
            CancelCause::ReaderGone,
            CancelCause::Interrupted,
            CancelCause::Cancelled,
            CancelCause::TimedOut,
            CancelCause::Terminated,
            CancelCause::Aborted,
        ]
        .into_iter()
        .map(|cause| {
            (
                Escape::Stopped(Stop {
                    cause,
                    owner: None,
                    at: None,
                }),
                EscapedLoad::Stopped(cause),
            )
        });
        for (escape, cause) in exits.chain(stops) {
            let error = load::<()>("--extend", || Err(Break::Escape(escape))).unwrap_err();
            assert_eq!(
                error.reason,
                Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Policy(
                    PolicyFailure::EscapedLoad(PolicyFailureEscapedLoad {
                        flag: "--extend".into(),
                        cause
                    })
                ),)))
            );
        }
    }

    #[test]
    fn policy_source_failure_keeps_its_compile_summary_and_report_only_rejection() {
        let mut shell = ral_core::test_helper::core_shell();
        let ctx = FreezeCtx::at(EnvVars::new(), ral_core::test_helper::process_cwd());
        let origin = SourceOrigin::named("<broken-policy>");
        for (source, parse) in [("let =", true), ("return [net: 1]", false)] {
            let error = load("--restrict", || {
                ral_core::shell::load::profile::load_capabilities_from_str(
                    &Mooring::adrift(),
                    &mut shell,
                    source,
                    &origin,
                    &ctx,
                )
            })
            .unwrap_err();
            let Reason::Compile(summary) = &error.reason else {
                panic!("compile summary: {error:?}")
            };
            assert_eq!(summary.source, "<broken-policy>");
            assert_eq!(matches!(summary.phase, CompilePhase::Parse), parse);
            assert!(error.rejection().is_some());
            assert_eq!(
                error.context(),
                Some("--restrict: capability file <broken-policy>")
            );
        }
    }
}
