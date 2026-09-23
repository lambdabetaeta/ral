//! Startup-file sourcing, run inside the boot door: the login profiles, then
//! the rc.
//!
//! Each kind of file has one contract on its return value: a profile is a
//! script sourced for its effects and returns `()`; the rc is a
//! configuration expression returning a record or a map. An `exit` in either
//! ends the session with its status. Any other failure is reported and the
//! boot goes on — a broken startup file must not strand the user at no shell.

use ral_core::source::Span;
use ral_core::types::{Break, DefaultPolicy, Escape, HookName, HookSig, Map, Mooring, Settled};
use ral_core::{Shell, Value, diagnostic};

use super::{RcSettings, apply_rc_config, create_default_rc, find_ralrc};

/// Why a startup file stopped.
#[derive(Debug, PartialEq)]
enum Stop {
    Exit(i32),
    Failed(String),
}

/// Report a failure and carry on; an `exit` ends the boot.
fn tolerate<T>(outcome: Result<T, Stop>, fallback: T) -> Settled<T> {
    match outcome {
        Ok(v) => Ok(v),
        Err(Stop::Exit(code)) => Err(Break::Escape(Escape::Exit(code))),
        Err(Stop::Failed(msg)) => {
            diagnostic::cmd_error("ral", &msg);
            Ok(fallback)
        }
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
        let user = ral_core::path::config::home_dot(".ral_profile")
            .map(|p| p.to_string_lossy().into_owned());
        for path in [Some("/etc/ral/profile".to_string()), user]
            .into_iter()
            .flatten()
        {
            if ral_core::path::exists(&path) {
                tolerate(source_profile(&path, mooring, shell), ())?;
            }
        }
    }
    let rc = find_ralrc().or_else(|| {
        let path = create_default_rc()?;
        eprintln!("note: created {path}");
        Some(path)
    });
    match rc {
        Some(path) => tolerate(source_rc(&path, mooring, shell), RcSettings::default()),
        None => Ok(RcSettings::default()),
    }
}

/// Anything but `()` is refused, a configuration map included: discarding it
/// would let a misplaced `[theme: …]` do nothing without a word.
fn source_profile(path: &str, mooring: &Mooring, shell: &mut Shell) -> Result<(), Stop> {
    match evaluate(path, None, mooring, shell)? {
        Value::Unit => Ok(()),
        v => Err(Stop::Failed(format!(
            "{path}: profile must return (); got {} — configuration belongs in the rc file",
            v.type_name()
        ))),
    }
}

/// Apply the rc's map and register its `startup` block. A broken file or a
/// refused map leaves the defaults; a startup block that will not register
/// keeps the settings the map applied.
fn source_rc(path: &str, mooring: &Mooring, shell: &mut Shell) -> Result<RcSettings, Stop> {
    let pairs = rc_config(path, mooring, shell)?;
    let (mut settings, startup) = apply_rc_config(pairs, mooring, shell)
        .map_err(|msg| Stop::Failed(format!("{path}: {msg}")))?;
    if let Some(block) = startup {
        match shell.register_hook(
            HookName::session("startup"),
            block,
            HookSig::Prompt,
            DefaultPolicy::denied(),
            Span::synthetic(),
        ) {
            Ok(()) => settings.startup = true,
            Err(e) => diagnostic::cmd_error("ral", &format!("{path}: startup: {e}")),
        }
    }
    Ok(settings)
}

fn rc_config(path: &str, mooring: &Mooring, shell: &mut Shell) -> Result<Map, Stop> {
    let contract = ral_core::typecheck::contract::declared(ral_core::typecheck::Form::Rc);
    match evaluate(path, Some(contract), mooring, shell)? {
        Value::Map(pairs) => Ok(pairs),
        other => Err(Stop::Failed(format!(
            "{path}: rc file must return a record or a map, e.g. `[edit_mode: 'vi']` or `[:]`; \
             got {} — does the file end with `return [...]`?",
            other.type_name()
        ))),
    }
}

/// Read, check, and evaluate one startup file for its caller's contract.
///
/// Checked against the live session, so an earlier file's bindings are in
/// scope for a later one, and compiled against the `FileId` `evaluate_checked`
/// registers the text under, so the spans of what the file defines keep
/// naming it for the whole session. `contract` holds the returned row to a
/// declared table in the same check.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:config-read] reads an rc/profile file during session boot; not turn-time model I/O"
)]
fn evaluate(
    path: &str,
    contract: Option<ral_core::typecheck::ReturnContract>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Result<Value, Stop> {
    let src = std::fs::read_to_string(path).map_err(|e| Stop::Failed(format!("{path}: {e}")))?;
    let src = ral_core::source::normalize_source_text(src);
    let file = shell.sources().next_id();
    let annotated = match ral_core::compile_and_typecheck(
        &src,
        shell.session_schemes(),
        file,
        path,
        contract,
    ) {
        Ok(annotated) => annotated,
        Err(ral_core::CompileError::Parse(e)) => {
            return Err(Stop::Failed(format!("{path}: {e}")));
        }
        Err(ral_core::CompileError::Types(errs)) => {
            eprint!(
                "{}",
                diagnostic::format_type_errors_ariadne(path, &src, &errs)
            );
            return Err(Stop::Failed(format!("{path}: skipped due to type errors")));
        }
    };
    let comp = std::sync::Arc::new(annotated);
    match ral_core::builtins::modules::evaluate_checked(mooring, shell, &comp, &src, path) {
        Ok(v) => Ok(v),
        Err(Break::Error(e)) => {
            eprint!(
                "{}",
                diagnostic::format_runtime_error_auto(shell.sources(), &e, None)
            );
            Err(Stop::Failed(format!(
                "{path}: sourcing stopped at the error above"
            )))
        }
        Err(Break::Escape(Escape::Exit(code))) => Err(Stop::Exit(code)),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;

    /// Write `src` as a startup file, returning the tempdir (keeping the
    /// file alive) and its path.
    fn startup_file(src: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup.ral");
        std::fs::write(&path, src).unwrap();
        let path = path.to_str().unwrap().to_owned();
        (dir, path)
    }

    fn booted_shell() -> Shell {
        ral_core::boot::boot_shell(
            ral_core::io::TerminalState::default(),
            &crate::PRELUDE,
            &ral_core::HostSurface::default(),
        )
    }

    fn profile(src: &str) -> Result<(), Stop> {
        let (_dir, path) = startup_file(src);
        source_profile(&path, &Mooring::adrift(), &mut booted_shell())
    }

    fn rc(src: &str) -> Result<Map, Stop> {
        let (_dir, path) = startup_file(src);
        rc_config(&path, &Mooring::adrift(), &mut booted_shell())
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
        let err = failure(profile("return [edit_mode: 'vi']\n"));
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

    #[test]
    fn rc_returning_unit_is_rejected() {
        let err = failure(rc("return ()\n"));
        assert!(
            err.contains("rc file must return a record or a map") && err.contains("got Unit"),
            "{err}"
        );
    }

    /// A last phrase that is a `let` yields `Unit`, as `return ()` does: the
    /// easy accident of forgetting the `return`.
    #[test]
    fn rc_ending_in_let_is_rejected() {
        let err = failure(rc("let x = 1\n"));
        assert!(
            err.contains("rc file must return a record or a map") && err.contains("got Unit"),
            "{err}"
        );
    }

    /// A malformed literal rc key is a type error, caught before the file
    /// runs at all.
    #[test]
    fn rc_bad_literal_field_is_a_type_error() {
        let err = failure(rc("return [edit_mode: 42]\n"));
        assert!(err.contains("skipped due to type errors"), "{err}");
    }

    #[test]
    fn rc_well_typed_literal_field_sources_cleanly() {
        assert!(rc("return [edit_mode: 'vi']\n").is_ok());
    }
}
