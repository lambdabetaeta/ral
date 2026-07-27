//! Load a capability profile from a `.ral` script.
//!
//! A profile is a ral script whose terminal expression is a map shaped
//! like the argument of `grant [...] { body }`.  This module is a thin
//! composition of two existing surfaces:
//!
//! - [`crate::builtins::modules::evaluate_source`] — parse + elaborate +
//!   evaluate.  All source-eval machinery lives there; we never
//!   re-implement it.
//! - [`crate::capability::decode_capability_map`] — walk the
//!   resulting `Value::Map` into a frozen `Capabilities`, resolving every
//!   sigil against a `FreezeCtx`.  All capability decoding lives there; we
//!   never re-implement it.
//!
//! Each profile is frozen at load against the caller-supplied `FreezeCtx`,
//! so a composed ceiling is the `meet` of already-resolved bundles.  Both
//! the ral CLI's `--capabilities` flag and exarch's base+extend+restrict
//! orchestrator consume this layer.

use std::path::Path;

use crate::types::{Break, Capabilities, Mooring, Settled, Shell, sig};

/// Evaluate `source` as a `.ral` script and walk its terminal value
/// into a frozen [`Capabilities`], resolving sigils against `ctx`.
///
/// `virtual_path` labels the file in error messages and in the
/// cycle-detection stack — pass the absolute path for files on disk, or
/// a synthetic identifier (`<built-in:NAME>`) for embedded profiles.
///
/// # Errors
/// Returns `Err` if evaluating `source` fails (a parse, elaboration, or
/// runtime error, wrapped with `virtual_path`), or if its terminal value
/// does not decode as a capability map (a non-map value, an unknown key, or
/// a sigil that fails to freeze). A propagated escape (`exit`, a stopped
/// child) passes through unchanged.
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

/// Read `path` from disk and forward to [`load_capabilities_from_str`].
///
/// # Errors
/// Returns `Err` if the file cannot be read (`file not found`, or any other
/// IO failure wrapped with the path), or for any failure of
/// [`load_capabilities_from_str`] on its contents.
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

/// Load, `meet`-compose, and install a session-wide capability ceiling.
///
/// Load each profile in `paths`, freezing each against the session's
/// `$HOME` and working directory, compose them left-to-right by `meet`
/// (each successive file narrows authority), and push the result onto
/// `shell` as a permanent session-wide ceiling.  No-op when `paths` is
/// empty.
///
/// The freeze context is built once and shared, so every profile resolves
/// its sigils against the same home and cwd.  An `xdg:` path that escapes
/// `$HOME` is rejected at the profile that names it.  `audit` propagates
/// upward through `meet`, so any profile declaring `audit: true` makes the
/// whole session audit.
///
/// A load failure carries a neutral mechanism message; a caller prepends
/// its own provenance (the `--capabilities` flag, a config key).
///
/// # Errors
/// Returns `Err` at the first profile that fails to load — a missing file,
/// or a decode/freeze error (an `xdg:` path escaping `$HOME`) — or forwards
/// an escape raised while a profile evaluates.
pub fn apply_session_profiles(
    mooring: &Mooring,
    shell: &mut Shell,
    paths: &[std::path::PathBuf],
) -> Settled<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let home = crate::path::home_from_env();
    let cwd = shell.cwd();
    let ctx = crate::path::sigil::FreezeCtx {
        home: &home,
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
            home: "/h",
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
        // Other dimensions stay None — no opinion → inherits caller.
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

    /// A non-map terminal value is a programmer error in the profile.
    /// The error names the file so the user can find the script.
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

    /// Unknown top-level keys are caught by the shared walker — no
    /// silent drop, matching the `deny_unknown_fields` discipline the
    /// schema enforces.
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

    /// An escape raised while a profile evaluates (`exit`) propagates
    /// unchanged rather than collapsing into an error string.
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
}
