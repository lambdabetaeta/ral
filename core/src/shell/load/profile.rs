//! Load a capability profile — a `.ral` script ending in `grant`'s options
//! written out, nested tables included (SPEC §15.3).
//!
//! Evaluation is [`super::evaluate_closed`], decoding is
//! [`decode_capability_map`]; this file only joins them.
//! Places freeze here, so a composed ceiling is the `meet` of already-resolved
//! bundles.  Entered by the ral CLI's `--capabilities` flag and by exarch's
//! base/extend/restrict orchestrator.

use std::path::Path;

use crate::authority::grant::freeze::FreezeCtx;
use crate::authority::policy::{Fragment, GrantLayer};
use crate::fact::{InvalidCause, IoOperation, IoSubject};
use crate::path::Reading;
use crate::shell::grant::decode_capability_map;
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Break, Error, Mooring, Settled, Shell};
use crate::source::{SourceFile, SourceOrigin};
use crate::text::quote_native;

/// Evaluate `source` and decode its terminal value into a resolved
/// [`GrantLayer`], walking places against `ctx`.
///
/// `origin` names the file in errors and anchors its imports; embedded
/// profiles have a virtual origin (`<built-in:NAME>`).
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
    let options = crate::typecheck::table::declared(crate::typecheck::Form::Grant);
    let prefix = format!("capability file {origin}");
    let value = super::evaluate_closed(mooring, shell, source, origin, Some(options))
        .map_err(|b| b.map_error(|e| e.with_context(&prefix)))?;
    decode_capability_map(&value, &prefix, ctx).map_err(Break::from)
}

/// Read `path` and forward to [`load_capabilities_from_str`], identified by
/// its real path so imports detect cycles through alternate spellings.
///
/// `path` is anchored against the shell's cwd, not the process's, and the
/// bytes are read through the very file the source is keyed by, judged for
/// reading at the shell's door, so the identity and the text are one file.
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
    let rp = shell.anchor(path);
    let located = (shell.locate::<Reading>(&rp))
        .map_err(|b| b.map_error(|e| e.with_context("capability file")))?;
    let file = SourceFile::new(located.real().as_path().to_path_buf()).map_err(|e| {
        invalid("load-capabilities", InvalidCause::Path((&e).into()))
            .with_context("capability file")
    })?;
    let reader = located.read().map_err(|e| {
        Error::io(
            IoOperation::Open,
            IoSubject::Path(quote_native(rp.as_path().as_os_str())),
            &e,
        )
        .with_context("capability file")
    })?;
    let source = std::io::read_to_string(reader).map_err(|e| {
        Error::io(
            IoOperation::Read,
            IoSubject::Path(quote_native(rp.as_path().as_os_str())),
            &e,
        )
        .with_context("capability file")
    })?;
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
    let ctx = shell.freeze_ctx();
    for path in paths {
        let layer = load_capabilities_from_path(mooring, shell, path, &ctx)?;
        shell.push_session_capabilities(layer);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{ExecAllow, FsAllow, FsOp};
    use crate::environ::EnvVars;

    fn shell() -> Shell {
        crate::test_helper::core_shell()
    }

    fn ctx() -> FreezeCtx {
        let home = crate::test_helper::Tree::nowhere().at("/h");
        FreezeCtx::at(
            EnvVars::from_pairs([("HOME", home)]).unwrap(),
            crate::test_helper::process_cwd(),
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
        let axes = layer.axes();
        assert!(
            axes.fs.is_none()
                && axes.net.is_none()
                && axes.gpu.is_none()
                && axes.editor.is_none()
                && axes.shell.is_none()
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

    fn load_compile_report(source: &str, label: &str) -> String {
        let mut shell = shell();
        let origin = SourceOrigin::named(label);
        let err =
            load_capabilities_from_str(&Mooring::adrift(), &mut shell, source, &origin, &ctx())
                .unwrap_err();
        match err {
            Break::Error(e) => {
                assert!(matches!(e.reason, crate::fact::Reason::Compile(_)));
                e.rejection()
                    .expect("rich compile report is separate from reason")
                    .render()
            }
            other @ Break::Escape(_) => panic!("unexpected: {other:?}"),
        }
    }

    /// A profile that returns a record is held to `grant`'s declared table as
    /// the form itself is, so the typo never reaches the walker.
    #[test]
    fn unknown_top_level_key_is_refused_before_the_file_runs() {
        let msg = load_compile_report("return [fss: [read: [`absolute /tmp]]]", "<test:typo>");
        assert!(msg.contains("'fss'"), "{msg}");
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

    /// A capability file under a denied name is refused at the shell's door,
    /// absent or not; elsewhere its absence is said.
    #[test]
    fn a_denied_absent_capability_file_is_refused_not_reported_absent() {
        use crate::authority::policy::{Capabilities, FsPolicy};
        use crate::path::FrozenPath;
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join("secret")).unwrap();
        let frozen = |p: &Path| FrozenPath::from_surface(crate::path::utf8(p).unwrap());
        let denying = crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![frozen(&root)],
                deny_paths: vec![frozen(&root.join("secret"))],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        let load = |shell: &mut Shell, name: &str| {
            let path = root.join(name);
            match load_capabilities_from_path(&Mooring::adrift(), shell, &path, &ctx()) {
                Err(Break::Error(e)) => e.describe().into_owned(),
                other => panic!("{name} must not load, got {other:?}"),
            }
        };
        shell().with_layer(denying, |shell| {
            for name in ["secret/p.ral", "secret/gone/p.ral"] {
                let message = load(shell, name);
                assert!(message.contains("denied by grant"), "{name}: {message}");
            }
            let message = load(shell, "gone/p.ral");
            assert!(message.ends_with("no such file or directory"), "{message}");
        });
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
            .set_env_var("HOME", home.path().to_string_lossy().into_owned())
            .unwrap();
        apply_session_profiles(&Mooring::adrift(), &mut shell, &[profile_path]).unwrap();

        let expected = home.path().join("data");
        let pushed = shell.context.grants.layers().last().unwrap();
        let fs = pushed.fs().expect("fs dimension present");
        assert!(fs.table(FsOp::Read).allows().any(
            |a| matches!(a, FsAllow::Prefix(gp) if Path::new(gp.written.as_str()) == expected)
        ));
    }
}
