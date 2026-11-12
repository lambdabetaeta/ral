//! Rc sourcing inside the boot builtin: a written, total record configures
//! the session. `exit` ends the session; any other failure is reported and
//! boot continues.

use ral_core::path::shown;
use ral_core::shell::runtime::{Break, Escape, Map, Mooring, Settled};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::{Shell, ValClos, View, io::terminal};
use std::path::Path;

use super::super::errfmt::report_failed_load;
use super::{RcPlan, RcSettings, rc};

/// Why a startup file stopped.
#[derive(Debug, PartialEq)]
enum Stop {
    Exit(u8),
    Failed(String),
    /// Failed, and already reported in full.
    Reported,
}

/// Report a failure and carry on; an `exit` ends the boot.
fn tolerate<T>(outcome: Result<T, Stop>, fallback: T) -> Settled<T> {
    match outcome {
        Ok(v) => Ok(v),
        Err(Stop::Exit(code)) => Err(Break::Escape(Escape::Exit(code))),
        Err(Stop::Failed(msg)) => {
            terminal::cmd_error("ral", &msg);
            Ok(fallback)
        }
        Err(Stop::Reported) => Ok(fallback),
    }
}

/// Source the rc, or keep defaults when it sets nothing.
pub(crate) fn source_rc_file(mooring: &Mooring, shell: &mut Shell) -> Settled<RcSettings> {
    match rc() {
        Some((file, text)) => tolerate(
            source_rc(file.path(), text, mooring, shell),
            RcSettings::default(),
        ),
        None => Ok(RcSettings::default()),
    }
}

/// Decode the rc's map, then commit it. A broken file or a refused map
/// leaves the defaults and changes nothing.
fn source_rc(
    path: &Path,
    text: String,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<RcSettings, Stop> {
    let plan = RcPlan::decode(&rc_config(path, text, mooring, shell)?, shell)
        .map_err(|msg| Stop::Failed(format!("{}: {msg}", shown(path))))?;
    plan.apply(mooring, shell).map_err(|b| match b {
        Break::Escape(Escape::Exit(code)) => Stop::Exit(code),
        halt => Stop::Failed(format!("{}: {halt}", shown(path))),
    })
}

fn rc_config(path: &Path, text: String, mooring: &Mooring, shell: &mut Shell) -> Result<Map, Stop> {
    let options = ral_core::typecheck::table::declared(ral_core::typecheck::Form::Rc);
    let value = evaluate(path, text, Some(options), mooring, shell)?;
    let View::Map(pairs) = value.view() else {
        unreachable!("an rc ends in its keys written out (SPEC §15.3)")
    };
    Ok(pairs.to_map())
}

/// Evaluate one startup file against the live session and its tail's options
/// table. The source keeps its native path and uses its shown path in messages.
fn evaluate(
    path: &Path,
    text: String,
    options: Option<&'static ral_core::typecheck::Table>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<ValClos, Stop> {
    let name = shown(path);
    let file = SourceFile::new(path).map_err(|e| Stop::Failed(e.to_string()))?;
    let src = ral_core::source::normalize_source_text(text);
    let origin = SourceOrigin::File(file);
    match ral_core::shell::load::extend_session(mooring, shell, &src, &origin, options) {
        Ok(v) => Ok(v),
        Err(Break::Error(e)) => {
            let ran = "sourcing stopped at the error above";
            report_failed_load(shell, &name, &e, Some(ran));
            Err(Stop::Reported)
        }
        Err(Break::Escape(Escape::Exit(code))) => Err(Stop::Exit(code)),
        Err(Break::Escape(stop @ Escape::Stopped(_))) => {
            Err(Stop::Failed(format!("{name}: {stop}")))
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::test_helper::rc_source;

    /// Write `src` as a startup file, returning the tempdir (keeping the
    /// file alive) and its path.
    fn startup_file(src: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup.ral");
        std::fs::write(&path, src).unwrap();
        (dir, path)
    }

    fn booted_shell() -> Shell {
        ral_core::shell::boot::boot_shell(
            ral_core::io::terminal::TerminalState::default(),
            &crate::PRELUDE,
            &ral_core::HostSurface::default(),
            ral_core::test_helper::process_cwd(),
        )
        .expect("a test host resolves its baseline")
    }

    fn rc(src: &str) -> Result<Map, Stop> {
        let (_dir, path) = startup_file(src);
        rc_config(
            &path,
            src.to_owned(),
            &Mooring::adrift(),
            &mut booted_shell(),
        )
    }

    #[test]
    fn rc_at_every_blank_sets_nothing() {
        let src = rc_source(&[]);
        let (_dir, path) = startup_file(&src);
        let mut shell = booted_shell();
        let settings = source_rc(&path, src, &Mooring::adrift(), &mut shell).unwrap();
        assert!(!settings.startup);
        assert!(!settings.bell);
        assert!(!shell.has_hook(&ral_core::shell::runtime::HookName::session("prompt")));
        assert_eq!(
            shell.stack_limit(),
            ral_core::shell::runtime::DEFAULT_STACK_LIMIT
        );
    }

    #[test]
    fn rc_returning_unit_is_refused() {
        for src in ["return ()\n", ""] {
            assert!(matches!(rc(src), Err(Stop::Reported)), "{src}");
        }
    }

    #[test]
    fn rc_ending_in_a_let_is_refused() {
        assert!(matches!(rc("let x = 1\n"), Err(Stop::Reported)));
    }

    #[test]
    fn rc_stopping_before_its_tail_propagates_without_decoding_or_applying() {
        for (head, expected) in [
            ("exit 23", Stop::Exit(23)),
            ("fail 'rc failed'", Stop::Reported),
        ] {
            let src = format!(
                "let rc-began = true\n{head}\n{}",
                rc_source(&[("startup", "`set { return () }")])
            );
            let (_dir, path) = startup_file(&src);
            let mut shell = booted_shell();
            let outcome = source_rc(&path, src, &Mooring::adrift(), &mut shell);
            assert!(matches!(outcome, Err(ref stop) if *stop == expected));
            assert!(
                shell.scope_lookup("rc-began").is_some(),
                "the rc ran before stopping"
            );
            assert!(!shell.has_hook(&ral_core::shell::runtime::HookName::session("startup")));
        }
    }

    /// A malformed literal rc key is a type error, caught before the file
    /// runs at all: the literal itself would evaluate without complaint.
    #[test]
    fn rc_bad_literal_field_is_a_type_error() {
        assert!(matches!(
            rc(&rc_source(&[("edit-mode", "42")])),
            Err(Stop::Reported)
        ));
    }
}
