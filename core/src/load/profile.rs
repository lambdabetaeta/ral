//! Load a capability profile — a `.ral` script whose terminal expression is
//! a map shaped like the argument of `grant [...] { body }`.
//!
//! Evaluation is [`super::evaluate_source`], decoding is
//! [`decode_capability_map`]; this file only joins them.
//! Places freeze here, so a composed ceiling is the `meet` of already-resolved
//! bundles.  Entered by the ral CLI's `--capabilities` flag and by exarch's
//! base/extend/restrict orchestrator.

use std::path::Path;

use crate::capability::{Fragment, GrantLayer, LocateError};
use crate::guard::decode_capability_map;
use crate::guard::freeze::FreezeCtx;
use crate::path::Reading;
use crate::source::{SourceFile, SourceOrigin};
use crate::types::{Break, Mooring, Settled, Shell, sig};

/// How a profile file that is not there is said to be, ahead of its name.
pub const NOT_FOUND: &str = "path does not exist";

/// Evaluate `source` and decode its terminal value into a resolved
/// [`GrantLayer`], walking places against `ctx`.
///
/// `origin` names the file in errors and keys the cycle stack — virtual
/// (`<built-in:NAME>`) for embedded profiles.
///
/// # Errors
/// A compile, runtime, or decode failure, named with `origin`; an escape
/// (`exit`, a stopped child) passes through untouched.
pub fn load_capabilities_from_str(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    origin: &SourceOrigin,
    ctx: &FreezeCtx,
) -> Settled<GrantLayer> {
    // A profile's dimensions are `grant`'s own options, so the file's returned
    // row is held to the same table the form is.
    let contract = crate::typecheck::contract::declared(crate::typecheck::Form::Grant);
    let value = super::evaluate_source(mooring, shell, source, origin, Some(contract))
        .map_err(|e| wrap(origin, e))?;
    let prefix = format!("capability file {origin}");
    decode_capability_map(&value, &prefix, ctx).map_err(Break::from)
}

/// Read `path` and forward to [`load_capabilities_from_str`], keyed by its
/// real path so two spellings of one file share a cycle-stack entry.
///
/// `path` is anchored against the shell's cwd, not the process's, and the
/// bytes are read through the very file the source is keyed by, judged for
/// reading under the shell's stack, so the identity and the text are one file.
///
/// # Errors
/// Its real path is not UTF-8, the grant refuses the file, it cannot be read,
/// or any failure of [`load_capabilities_from_str`].
pub fn load_capabilities_from_path(
    mooring: &Mooring,
    shell: &mut Shell,
    path: &Path,
    ctx: &FreezeCtx,
) -> Settled<GrantLayer> {
    let display = crate::path::shown(path);
    let failed = |e: &dyn std::fmt::Display| sig(format!("capability file {display}: {e}"));
    let unread = |e: &std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => sig(format!("{NOT_FOUND}: {display}")),
        _ => failed(e),
    };
    let located = (shell.context.grants)
        .locate::<Reading>(shell.floor(), &shell.anchor(path))
        .map_err(|e| match &e {
            LocateError::Io(io) => unread(io),
            LocateError::Refused(refused) => failed(refused),
        })?;
    let file = SourceFile::new(located.real().as_path().to_path_buf())
        .map_err(|e| sig(format!("capability file {e}")))?;
    let source = (located.read().and_then(std::io::read_to_string)).map_err(|e| unread(&e))?;
    let source = crate::source::normalize_source_text(source);
    load_capabilities_from_str(mooring, shell, &source, &SourceOrigin::File(file), ctx)
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
    origin: &SourceOrigin,
    ctx: &FreezeCtx,
) -> Settled<Fragment> {
    load_capabilities_from_str(mooring, shell, source, origin, ctx).map(Fragment::of)
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
/// A front-end passes its command-line paths already anchored at the
/// invocation cwd: by the time they load, the startup files and every earlier
/// profile have run and may have moved the shell's cwd, against which a
/// relative path would anchor.
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

fn wrap(origin: &SourceOrigin, e: Break) -> Break {
    match e {
        Break::Error(err) => Break::Error(err.context(format!("capability file {origin}"))),
        other @ Break::Escape(_) => other,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::capability::{ExecAllow, ExecDeny, FsAllow, FsOp};

    fn shell() -> Shell {
        crate::test_helper::core_shell()
    }

    fn ctx() -> FreezeCtx {
        FreezeCtx::at(
            std::iter::once(("HOME", "/h")).collect::<crate::types::EnvVars>(),
            crate::path::CwdPath::new("/".into()).expect("a fixture cwd is absolute"),
        )
    }

    #[test]
    fn loads_minimal_exec_only_profile() {
        let mut shell = shell();
        let layer = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [allow: [`name ls]]]",
            &SourceOrigin::named("<test:minimal>"),
            &ctx(),
        )
        .unwrap();
        let exec = layer.exec().expect("exec dimension present");
        assert!(
            exec.allows().any(|a| matches!(
                a,
                ExecAllow::Tool(name) | ExecAllow::File { found_as: Some(name), .. } if name == "ls"
            )),
            "{exec:?}"
        );
        assert!(exec.denies().next().is_none(), "{exec:?}");
        // `None` is no opinion, so an unmentioned dimension inherits the caller.
        let summary = layer
            .summary()
            .expect("an exec that runs `ls` shows itself");
        assert!(
            summary.fs.is_none()
                && summary.net.is_none()
                && summary.editor.is_none()
                && summary.shell.is_none()
        );
    }

    #[test]
    fn deny_target_produces_sticky_deny_policy() {
        let mut shell = shell();
        let layer = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return [exec: [deny: [`name bash]]]",
            &SourceOrigin::named("<test:deny>"),
            &ctx(),
        )
        .unwrap();
        let exec = layer.exec().unwrap();
        assert!(
            exec.denies().any(|d| *d == ExecDeny::Name("bash".into())),
            "{exec:?}"
        );
        assert!(exec.allows().next().is_none(), "{exec:?}");
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
                &SourceOrigin::named("<test:fragment>"),
                &ctx(),
            )
            .unwrap()
        };
        let positive = GrantLayer::root().extend(load("return [exec: [allow: [`name ls]]]"));
        assert!(!positive.is_restrictive(), "{positive:?}");
        let excluding = GrantLayer::root().extend(load("return [exec: [deny: [`name rm]]]"));
        assert!(excluding.is_restrictive(), "{excluding:?}");
        assert!(
            excluding
                .exec()
                .is_some_and(|e| e.allows().any(|a| matches!(a, ExecAllow::Any)))
        );
    }

    #[test]
    fn non_map_return_value_errors_with_file_path() {
        let mut shell = shell();
        let err = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return 42",
            &SourceOrigin::named("<test:nonmap>"),
            &ctx(),
        )
        .unwrap_err();
        let msg = match err {
            Break::Error(e) => e.message,
            other @ Break::Escape(_) => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("<test:nonmap>"), "should name the file: {msg}");
    }

    fn load_err_message(source: &str, label: &str) -> String {
        let mut shell = shell();
        let origin = SourceOrigin::named(label);
        let err =
            load_capabilities_from_str(&Mooring::adrift(), &mut shell, source, &origin, &ctx())
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
        let layer = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "return ()",
            &SourceOrigin::named("<test:unit>"),
            &ctx(),
        )
        .unwrap();
        assert_eq!(layer, GrantLayer::root());
    }

    #[test]
    fn exit_in_profile_propagates_as_escape() {
        let mut shell = shell();
        let err = load_capabilities_from_str(
            &Mooring::adrift(),
            &mut shell,
            "exit 3",
            &SourceOrigin::named("<test:exit>"),
            &ctx(),
        )
        .unwrap_err();
        match err {
            Break::Escape(crate::types::Escape::Exit(3)) => {}
            other => panic!("expected Escape::Exit(3), got {other:?}"),
        }
    }

    /// An env-scoped `HOME` reaches `home` freezing here exactly as it reaches
    /// `grant`'s: one home for both freezes.
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
        let pushed = shell.context.grants.layers().last().unwrap();
        let fs = pushed.fs().expect("fs dimension present");
        assert!(fs.table(&FsOp::Read).allows().any(
            |a| matches!(a, FsAllow::Prefix(gp) if Path::new(gp.written.as_str()) == expected)
        ));
    }
}
