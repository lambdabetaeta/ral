//! Load a capability profile — a `.ral` script whose terminal expression is
//! a map shaped like the argument of `grant [...] { body }`.
//!
//! Evaluation is [`crate::builtins::modules::evaluate_source`], decoding is
//! [`crate::capability::decode_capability_map`]; this file only joins them.
//! Sigils freeze here, so a composed ceiling is the `meet` of already-resolved
//! bundles.  Entered by the ral CLI's `--capabilities` flag and by exarch's
//! base/extend/restrict orchestrator.

use std::path::Path;

use crate::types::{Break, Capabilities, Mooring, Settled, Shell, sig};

/// Evaluate `source` and decode its terminal value into a frozen
/// [`Capabilities`], resolving sigils against `ctx`.
///
/// `virtual_path` names the file in errors and keys the cycle stack —
/// synthetic (`<built-in:NAME>`) for embedded profiles.
///
/// # Errors
/// A compile, runtime, or decode failure, named with `virtual_path`; an escape
/// (`exit`, a stopped child) passes through untouched.
pub fn load_capabilities_from_str(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    virtual_path: &str,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Settled<Capabilities> {
    let value = crate::builtins::modules::evaluate_source(mooring, shell, source, virtual_path)
        .map_err(|e| wrap(virtual_path, e))?;
    let prefix = format!("capability file {virtual_path}");
    crate::capability::decode_capability_map(&value, &prefix, ctx).map_err(Break::from)
}

/// Read `path` and forward to [`load_capabilities_from_str`], keyed by its
/// canonical form so two spellings of one file share a cycle-stack entry.
///
/// # Errors
/// The file cannot be read, or any failure of [`load_capabilities_from_str`].
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:cap-load] reads a capability-policy file from disk to configure the sandbox; policy/configuration loading at setup, not turn-time model data I/O, raises no surface card."
)]
pub fn load_capabilities_from_path(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Settled<Capabilities> {
    let display = path.to_string_lossy().into_owned();
    let source = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            sig(format!("file not found: {display}"))
        } else {
            sig(format!("capability file {display}: {e}"))
        }
    })?;
    let source = crate::source::normalize_source_text(source);
    let abs = shell
        .resolve(&path.to_string_lossy())
        .canonicalise_strict()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(display);
    load_capabilities_from_str(mooring, shell, &source, &abs, ctx)
}

/// Load each profile in `paths`, `meet`-fold left to right (each file narrows
/// authority), and push the result as a permanent session-wide ceiling.
///
/// One `FreezeCtx` serves the whole fold, so every profile resolves its sigils
/// against the same home and cwd, and an `xdg:` path escaping `$HOME` is
/// rejected at the profile that names it.  Failures carry a bare mechanism
/// message; the caller prepends provenance (`--capabilities`, a config key).
/// The home is the shell's effective `$HOME` — the override chain — the same
/// anchor `eval_grant` freezes against.
///
/// # Errors
/// The first profile that fails to load or decode; an escape raised while a
/// profile evaluates propagates.
pub fn apply_session_profiles(
    mooring: &Mooring,
    shell: &mut Shell,
    paths: &[std::path::PathBuf],
) -> Settled<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let home = shell.context.home();
    let cwd = shell.cwd();
    let ctx = crate::path::sigil::FreezeCtx {
        home: home.as_deref(),
        cwd: &cwd,
    };
    let mut composed = Capabilities::default();
    for path in paths {
        composed = composed.meet(load_capabilities_from_path(mooring, shell, path, &ctx)?);
    }
    shell.push_session_capabilities(composed);
    Ok(())
}

fn wrap(virtual_path: &str, e: Break) -> Break {
    match e {
        Break::Error(err) => Break::Error(crate::types::Error::new(
            format!("capability file {virtual_path}: {}", err.message),
            err.exit_code(),
        )),
        other @ Break::Escape(_) => other,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::types::ExecPolicy;

    fn shell() -> Shell {
        Shell::new(crate::io::TerminalState::default())
    }

    fn ctx() -> crate::path::sigil::FreezeCtx<'static> {
        crate::path::sigil::FreezeCtx {
            home: Some("/h"),
            cwd: std::path::Path::new("/"),
        }
    }

    #[test]
    fn loads_minimal_exec_only_profile() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [ls: 'allow']]",
            "<test:minimal>",
            &ctx(),
        )
        .unwrap();
        let exec = caps.exec.expect("exec dimension present");
        assert_eq!(exec.literals.get("ls"), Some(&ExecPolicy::Allow));
        // `None` is no opinion, so an unmentioned dimension inherits the caller.
        assert!(
            caps.fs.is_none()
                && caps.net.is_none()
                && caps.editor.is_none()
                && caps.shell.is_none()
        );
    }

    #[test]
    fn audit_flag_propagates_through_loader() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [audit: true, net: false]",
            "<test:audit>",
            &ctx(),
        )
        .unwrap();
        assert!(caps.audit);
        assert_eq!(caps.net, Some(false));
    }

    #[test]
    fn deny_string_produces_sticky_deny_policy() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [bash: 'deny']]",
            "<test:deny>",
            &ctx(),
        )
        .unwrap();
        assert_eq!(
            caps.exec.unwrap().literals.get("bash"),
            Some(&ExecPolicy::Deny)
        );
    }

    #[test]
    fn non_map_return_value_errors_with_file_path() {
        let mut shell = shell();
        let err = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return 42",
            "<test:nonmap>",
            &ctx(),
        )
        .unwrap_err();
        let msg = match err {
            Break::Error(e) => e.message,
            other @ Break::Escape(_) => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("<test:nonmap>"), "should name the file: {msg}");
    }

    /// The shared walker rejects a typo'd key rather than dropping it silently.
    #[test]
    fn unknown_top_level_key_errors() {
        let mut shell = shell();
        let err = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [fss: [read: ['/tmp']]]",
            "<test:typo>",
            &ctx(),
        )
        .unwrap_err();
        let msg = match err {
            Break::Error(e) => e.message,
            other @ Break::Escape(_) => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("unknown key 'fss'"), "{msg}");
    }

    #[test]
    fn exit_in_profile_propagates_as_escape() {
        let mut shell = shell();
        let err = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "exit 3",
            "<test:exit>",
            &ctx(),
        )
        .unwrap_err();
        match err {
            Break::Escape(crate::types::Escape::Exit(3)) => {}
            other => panic!("expected Escape::Exit(3), got {other:?}"),
        }
    }

    /// An env-scoped `HOME` reaches `~` freezing here exactly as it reaches
    /// `grant`'s: one home for both freeze doors.
    #[test]
    fn session_profiles_freeze_tilde_against_the_shell_home() {
        let home = tempfile::tempdir().unwrap();
        let profile_dir = tempfile::tempdir().unwrap();
        let profile_path = profile_dir.path().join("profile.ral");
        std::fs::write(&profile_path, "return [fs: [read: ['~/data']]]").unwrap();

        let mut shell = shell();
        shell
            .context
            .set_env_var("HOME", home.path().to_string_lossy().into_owned());
        apply_session_profiles(&Mooring::adrift(), &mut shell, &[profile_path]).unwrap();

        let expected = home.path().join("data");
        let pushed = shell.context.grants.iter().last().unwrap();
        let read_prefixes = &pushed
            .fs
            .as_ref()
            .expect("fs dimension present")
            .read_prefixes;
        assert!(
            read_prefixes
                .iter()
                .any(|p| Path::new(p.as_str()) == expected)
        );
    }
}
