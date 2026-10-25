//! Startup-file sourcing, run inside the boot builtin: the login profiles, then
//! the rc.
//!
//! Each kind of file has one contract on its return value: a profile is a
//! script sourced for its effects and returns `()`; the rc is a
//! configuration expression returning a record, or `()` for nothing to set. An `exit` in either
//! ends the session with its status. Any other failure is reported and the
//! boot goes on — a broken startup file must not strand the user at no shell.

use ral_core::own::UserFile;
use ral_core::path::shown;
use ral_core::shell::runtime::{Break, Escape, Map, Mooring, Settled};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::{Shell, Value, io::terminal};
use std::path::Path;

use super::super::errfmt::report_failed_load;
use super::{RcPlan, RcSettings, rc};

/// Why a startup file stopped.
#[derive(Debug, PartialEq)]
enum Stop {
    Exit(i32),
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

/// Source the login profiles when `login`, `/etc/ral/profile` then
/// `~/.ral_profile`, and then the rc — `$XDG_CONFIG_HOME/ral/rc` or
/// `~/.ralrc`, created from the default skeleton when neither exists.
/// What the rc settled; the defaults when it settled nothing.
pub(crate) fn source_startup_files(
    login: bool,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<RcSettings> {
    if login {
        for profile in [
            Some(UserFile::fixed("/etc/ral", "profile")),
            UserFile::home(".ral_profile"),
        ]
        .into_iter()
        .flatten()
        {
            let text = profile.read().map_err(|e| Stop::Failed(e.to_string()));
            if let Some(text) = tolerate(text, None)? {
                tolerate(source_profile(profile.path(), text, mooring, shell), ())?;
            }
        }
    }
    match rc() {
        Some((file, text)) => tolerate(
            source_rc(file.path(), text, mooring, shell),
            RcSettings::default(),
        ),
        None => Ok(RcSettings::default()),
    }
}

/// Anything but `()` is refused, a configuration map included: discarding it
/// would let a misplaced `[theme: …]` do nothing without a word.
fn source_profile(
    path: &Path,
    text: String,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<(), Stop> {
    match evaluate(path, text, None, mooring, shell)? {
        Value::Unit => Ok(()),
        v => Err(Stop::Failed(format!(
            "{}: profile must return (); got {}; configuration belongs in the rc file",
            shown(path),
            v.type_name()
        ))),
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
    let contract = ral_core::typecheck::contract::declared(ral_core::typecheck::Form::Rc);
    match evaluate(path, text, Some(contract), mooring, shell)? {
        Value::Map(pairs) => Ok(pairs),
        Value::Unit => Ok(Map::new()),
        other => Err(Stop::Failed(format!(
            "{}: rc file must return a record, a map, or `()` for nothing to set, \
             e.g. `[edit-mode: 'vi']`; got {}; does the file end with `return [...]`?",
            shown(path),
            other.type_name()
        ))),
    }
}

/// Evaluate the `text` of one startup file for its caller's contract, checked
/// against the live session, so an earlier file's bindings are in scope for a
/// later one. `contract` holds the returned row to a declared table in the
/// same check.  The source is identified by the file's native `path`, and
/// named by its shown one in every message.
fn evaluate(
    path: &Path,
    text: String,
    contract: Option<ral_core::typecheck::ReturnContract>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<Value, Stop> {
    let name = shown(path);
    let file = SourceFile::new(path).map_err(|e| Stop::Failed(e.to_string()))?;
    let src = ral_core::source::normalize_source_text(text);
    let origin = SourceOrigin::File(file);
    match ral_core::shell::load::evaluate_source(mooring, shell, &src, &origin, contract) {
        Ok(v) => Ok(v),
        Err(Break::Error(e)) => {
            let ran = "sourcing stopped at the error above";
            report_failed_load(shell, &name, &e, Some(ran));
            Err(Stop::Reported)
        }
        Err(Break::Escape(Escape::Exit(code))) => Err(Stop::Exit(code)),
        Err(Break::Escape(Escape::Stopped { cause, .. })) => {
            Err(Stop::Failed(format!("{name}: {}", cause.message())))
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

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

    fn profile(src: &str) -> Result<(), Stop> {
        let (_dir, path) = startup_file(src);
        source_profile(
            &path,
            src.to_owned(),
            &Mooring::adrift(),
            &mut booted_shell(),
        )
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

    fn failure(outcome: Result<impl std::fmt::Debug, Stop>) -> String {
        match outcome {
            Err(Stop::Failed(msg)) => msg,
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    #[test]
    fn profile_returning_unit_sources_cleanly() {
        assert_eq!(profile("return ()\n"), Ok(()));
    }

    /// The error names the profile contract and points at the rc file.
    #[test]
    fn profile_returning_map_is_rejected() {
        let err = failure(profile("return [edit-mode: 'vi']\n"));
        assert!(err.contains("profile must return (); got Map"), "{err}");
        assert!(
            err.contains("configuration belongs in the rc file"),
            "{err}"
        );
    }

    /// `exit` in a startup file ends the boot with its status.
    #[test]
    fn exit_in_a_profile_is_an_exit() {
        assert_eq!(profile("exit 7\n"), Err(Stop::Exit(7)));
    }

    /// `()` is an rc with nothing to set, and so is a last phrase that is a
    /// `let`, which yields `Unit` as `return ()` does.
    #[test]
    fn rc_returning_unit_sets_nothing() {
        for src in ["return ()\n", "let x = 1\n"] {
            assert!(rc(src).unwrap().is_empty(), "{src}");
        }
    }

    /// The first-run skeleton is an rc that sets nothing.
    #[test]
    fn the_default_rc_sets_nothing() {
        assert!(rc(super::super::DEFAULT_RC).unwrap().is_empty());
    }

    #[test]
    fn a_decoded_rc_that_is_no_map_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("rc.json");
        std::fs::write(&json, "\"vi\"").unwrap();
        let err = failure(rc(&format!(
            "return !{{from-json < '{}'}}\n",
            ral_core::path::shown(&json)
        )));
        assert!(
            err.contains("rc file must return a record, a map, or `()`")
                && err.contains("got String"),
            "{err}"
        );
    }

    /// A malformed literal rc key is a type error, caught before the file
    /// runs at all: the literal itself would evaluate without complaint.
    #[test]
    fn rc_bad_literal_field_is_a_type_error() {
        assert!(matches!(
            rc("return [edit-mode: 42]\n"),
            Err(Stop::Reported)
        ));
    }

    #[test]
    fn rc_well_typed_literal_field_sources_cleanly() {
        assert!(rc("return [edit-mode: 'vi']\n").is_ok());
    }
}
