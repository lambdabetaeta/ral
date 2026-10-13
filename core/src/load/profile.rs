//! Load a capability profile — a `.ral` script whose terminal expression is
//! a map shaped like the argument of `grant [...] { body }`.
//!
//! Evaluation is [`super::evaluate_source`], decoding is
//! [`decode_capability_map`]; this file only joins them.
//! Places freeze here, so a composed ceiling is the `meet` of already-resolved
//! bundles.  Entered by the ral CLI's `--capabilities` flag and by exarch's
//! base/extend/restrict orchestrator.

use std::path::Path;

use crate::capability::{Capabilities, Fragment};
use crate::guard::decode_capability_map;
use crate::guard::freeze::FreezeCtx;
use crate::types::{Break, Mooring, Settled, Shell, sig};

/// Evaluate `source` and decode its terminal value into a frozen
/// [`Capabilities`], resolving places against `ctx`.
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
    ctx: &FreezeCtx,
) -> Settled<Capabilities> {
    // A profile's dimensions are `grant`'s own options, so the file's returned
    // row is held to the same table the form is.
    let contract = crate::typecheck::contract::declared(crate::typecheck::Form::Grant);
    let value = super::evaluate_source(mooring, shell, source, virtual_path, Some(contract))
        .map_err(|e| wrap(virtual_path, e))?;
    let prefix = format!("capability file {virtual_path}");
    decode_capability_map(&value, &prefix, ctx).map_err(Break::from)
}

/// Read `path` and forward to [`load_capabilities_from_str`], keyed by its
/// canonical form so two spellings of one file share a cycle-stack entry.
///
/// # Errors
/// The file cannot be read, or any failure of [`load_capabilities_from_str`].
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:cap-load] reads a capability-policy file from disk to configure the sandbox; policy/configuration loading at setup, not turn-time model data I/O, raises no surface card."
)]
pub fn load_capabilities_from_path(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    ctx: &FreezeCtx,
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

/// [`load_capabilities_from_str`] for an extension file: what the source states
/// is a [`Fragment`], in which an omitted axis states nothing.
///
/// # Errors
/// As [`load_capabilities_from_str`].
pub fn load_fragment_from_str(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    virtual_path: &str,
    ctx: &FreezeCtx,
) -> Settled<Fragment> {
    load_capabilities_from_str(mooring, shell, source, virtual_path, ctx).map(Fragment::of)
}

/// [`load_capabilities_from_path`] for an extension file.
///
/// # Errors
/// As [`load_capabilities_from_path`].
pub fn load_fragment_from_path(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    ctx: &FreezeCtx,
) -> Settled<Fragment> {
    load_capabilities_from_path(mooring, shell, path, ctx).map(Fragment::of)
}

/// Load each profile in `paths` and push each as its own permanent, session-wide layer.
///
/// Root first, and the stack folds them at every check, so no file's opinion
/// is ever flattened away by a sibling that is silent on the same key.
///
/// One `FreezeCtx` serves every load, so every profile resolves its places
/// against the same home and cwd, and an `xdg-*` place escaping `HOME` is
/// rejected at the profile that names it.  Failures carry a bare mechanism
/// message; the caller prepends provenance (`--capabilities`, a config key).
/// The home is the shell's effective `HOME` — the override chain — the same
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
    let ctx = FreezeCtx::of(shell);
    for path in paths {
        let layer = load_capabilities_from_path(mooring, shell, path, &ctx)?;
        shell.push_session_capabilities(layer);
    }
    Ok(())
}

fn wrap(virtual_path: &str, e: Break) -> Break {
    match e {
        Break::Error(err) => Break::Error(err.context(format!("capability file {virtual_path}"))),
        other @ Break::Escape(_) => other,
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures write a capability file to load"
)]
mod tests {
    use super::*;
    use crate::capability::{Evidence, ExecKey};

    fn shell() -> Shell {
        crate::test_helper::core_shell()
    }

    fn ctx() -> FreezeCtx {
        FreezeCtx {
            home: Some("/h".into()),
            cwd: "/".into(),
        }
    }

    #[test]
    fn loads_minimal_exec_only_profile() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [allow: [`name ls]]]",
            "<test:minimal>",
            &ctx(),
        )
        .unwrap();
        let exec = caps.exec.expect("exec dimension present");
        assert_eq!(
            exec.get(&ExecKey::Name("ls".into())),
            Some(&Evidence::ALLOW)
        );
        // `None` is no opinion, so an unmentioned dimension inherits the caller.
        assert!(
            caps.fs.is_none()
                && caps.net.is_none()
                && caps.editor.is_none()
                && caps.shell.is_none()
        );
    }

    #[test]
    fn deny_target_produces_sticky_deny_policy() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [deny: [`name bash]]]",
            "<test:deny>",
            &ctx(),
        )
        .unwrap();
        assert_eq!(
            caps.exec.unwrap().get(&ExecKey::Name("bash".into())),
            Some(&Evidence::DENY)
        );
    }

    /// A fragment extends a base that omits an axis to every request less its
    /// exclusions, and a positive-only one leaves the axis unrestricted.
    #[test]
    fn a_fragment_extends_by_role_not_by_omission() {
        let mut shell = shell();
        let mut load = |source: &str| {
            load_fragment_from_str(
                &Mooring::adrift(),
                &mut shell,
                source,
                "<test:fragment>",
                &ctx(),
            )
            .unwrap()
        };
        let positive = Capabilities::default().extend(load("return [exec: [allow: [`name ls]]]"));
        assert!(!positive.is_restrictive(), "{positive:?}");
        let excluding = Capabilities::default().extend(load("return [exec: [deny: [`name rm]]]"));
        assert!(excluding.is_restrictive(), "{excluding:?}");
        assert!(excluding.exec.is_some_and(|e| e.every()));
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

    fn load_err_message(source: &str, path: &str) -> String {
        let mut shell = shell();
        let err = load_capabilities_from_str(&Mooring::adrift(), &mut shell, source, path, &ctx())
            .unwrap_err();
        match err {
            Break::Error(e) => e.message,
            other @ Break::Escape(_) => panic!("unexpected: {other:?}"),
        }
    }

    /// A profile that returns a record is held to `grant`'s declared table as
    /// the form itself is, so the typo never reaches the walker.
    #[test]
    fn unknown_top_level_key_is_refused_before_the_file_runs() {
        let msg = load_err_message("return [fss: [read: [`absolute /tmp]]]", "<test:typo>");
        assert!(msg.contains("'fss'"), "{msg}");
    }

    /// A profile returning a map is refused statically: its keys are the
    /// table's labels, so it is written as a record.
    #[test]
    fn a_mapped_profile_is_refused_before_the_file_runs() {
        let msg = load_err_message(
            "return [\"fss\": [read: [`absolute /tmp]]]",
            "<test:typo-map>",
        );
        assert!(msg.contains("`grant` takes a record of settings"), "{msg}");
    }

    /// `()` is a profile with nothing to set: the lattice top.
    #[test]
    fn a_profile_returning_unit_is_the_empty_profile() {
        let mut shell = shell();
        let caps = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return ()",
            "<test:unit>",
            &ctx(),
        )
        .unwrap();
        assert_eq!(caps, Capabilities::default());
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

    /// An env-scoped `HOME` reaches `home` freezing here exactly as it reaches
    /// `grant`'s: one home for both freeze doors.
    #[test]
    fn session_profiles_freeze_home_against_the_shell_home() {
        let home = tempfile::tempdir().unwrap();
        let profile_dir = tempfile::tempdir().unwrap();
        let profile_path = profile_dir.path().join("profile.ral");
        std::fs::write(&profile_path, "return [fs: [read: [`home data]]]").unwrap();

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
