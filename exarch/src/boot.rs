//! Session boot: the shell every exarch engine starts from.
//! Nothing here runs per exchange — per-session disk state is
//! [`crate::record::AgentLog`].

use crate::app::{SCRATCH_VAR, seed_vars};
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::data::FOValue;
use ral_core::environ::EnvVars;
use ral_core::error::Error;
use ral_core::io::terminal::TerminalState;
use ral_core::protocol::{EngineSpec, Root};
use ral_core::shell::boot::boot_runtime;
use ral_core::{Runtime, Shell};

/// The process-level signal ceremony, once at each entry point that hosts an
/// identity engine: exarch's cancel chain layers over ral's handlers.
pub fn face_process_signals(terminal: &TerminalState) {
    ral_core::process::clear();
    ral_core::process::install_handlers();
    crate::signals::install();
    terminal.seat();
}

/// Exarch's process half, under either carrier: the dressed surface and
/// prelude, `detach` where the double fork exists, the agent library's docs.
///
/// # Errors
/// A host whose system reads cannot be resolved, which can run nothing.
pub fn engine_runtime(spec: &EngineSpec) -> Result<Runtime, Error> {
    let mut runtime = boot_runtime(
        spec.terminal,
        &shell_eval::PRELUDE,
        &builtins::host_surface(),
    )?;
    #[cfg(unix)]
    runtime.install_builtins(ral_core::shell::builtins::DETACH_BUILTIN);
    runtime.install_library_docs(crate::library::agent_library_docs());
    Ok(runtime)
}

/// Exarch's root-only recipe: the agent library, hints, and the root's env
/// seeded as bindings before the ledgers arm.
///
/// The scratch is the host's, named by the root, and the shell carries its
/// spelling, so that a `scratch` place in any grant frozen in this session
/// anchors to it, on the host and in a guest alike.  The root's attach is
/// authored by the host and is where it is read from, never the overlay, which
/// a `within [env: …]` could redirect.
///
/// A root whose config is not unit carries the ceiling its host authored for
/// this engine, a grant value, resolved here against the engine's own anchor
/// and pushed as a session layer: synod's guest names its places in the
/// guest's namespace.
///
/// # Errors
/// A root that names no scratch, or whose ceiling does not resolve.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn engine_root(root: &Root, shell: &mut Shell) -> Result<(), Error> {
    let Some(scratch) = root.env.get(SCRATCH_VAR) else {
        return Err(Error::new(ral_core::fact::Reason::Protocol(Box::new(
            ral_core::fact::ProtocolFailure::RootMissingScratch,
        )))
        .with_context("exarch engine"));
    };
    shell.set_scratch(scratch.as_str());
    if !matches!(root.config, FOValue::Unit) {
        // Freeze against the receiving root's env without moving its seeds
        // ahead of the library or changing when the session ledgers arm.
        let mut env = shell.env_overrides().clone();
        env.overlay(&root.env);
        let layer = shell
            .decode_grant_with_env(&root.config, "the root's ceiling", env)
            .map_err(|e| e.with_context("exarch engine"))?;
        shell.push_session_capabilities(layer);
    }
    crate::library::install_agent_library(&ral_core::shell::runtime::Mooring::adrift(), shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(shell);
    shell.set_exit_hints(ral_core::shell::runtime::ExitHints::from_text(
        include_str!("../../data/exit-hints.txt"),
    ));
    seed_vars(shell, &root.env);
    shell_eval::arm_session_ledgers(shell);
    Ok(())
}

/// Suppress ANSI colour at the source.  A tool call's stdout is a pipe, never a
/// TTY, so these only have to beat config that *forces* colour;
/// [`crate::agent::digest`]'s strip covers the tools that honour neither.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    let no_color = EnvVars::from_pairs([("NO_COLOR", "1"), ("CLICOLOR_FORCE", "0")])
        .expect("neither name is reserved");
    shell.overlay_env(&no_color);
}

#[cfg(all(test, unix))]
pub(crate) use testkit::test_attach;
#[cfg(test)]
pub(crate) use testkit::{test_shell, test_transport, test_transport_under};

/// The engines and shells a test boots: over the temp directory, naming it as
/// the scratch, so none holds a scratch directory of its own.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod testkit {
    use super::{SCRATCH_VAR, builtins, engine_root, engine_runtime};
    use ral_core::Shell;
    use ral_core::environ::EnvVars;
    use ral_core::protocol::{EngineSpec, Root};

    /// The engine and root a test attaches, the root naming the scratch.
    pub(crate) fn test_attach() -> (EngineSpec, Root) {
        let temp = std::env::temp_dir();
        (
            EngineSpec::new(
                builtins::INSTALLER_TAG,
                ral_core::path::EnginePath::Host(temp.clone()),
            ),
            Root::new(ral_core::path::EnginePath::Host(temp.clone())).with_env(
                EnvVars::from_pairs([(
                    SCRATCH_VAR.to_owned(),
                    ral_core::path::utf8(&temp)
                        .expect("a temp dir is Unicode")
                        .to_owned(),
                )])
                .unwrap(),
            ),
        )
    }

    /// A dressed shell for a test, over no scratch of its own.
    pub(crate) fn test_shell() -> Shell {
        let (spec, root) = test_attach();
        let mut shell = Shell::root(
            engine_runtime(&spec)
                .expect("a test host resolves its baseline")
                .into(),
            ral_core::test_helper::process_cwd(),
        );
        ral_core::shell::boot::seed_host_env(&mut shell);
        engine_root(&root, &mut shell).expect("test_attach names the scratch");
        shell
    }

    /// An identity engine for a test, booted through the one recipe.
    pub(crate) fn test_transport() -> ral_core::carrier::IdentityTransport {
        test_transport_under(ral_core::authority::policy::GrantStack::default())
    }

    /// An identity engine holding the test's session grants.
    pub(crate) fn test_transport_under(
        grants: ral_core::authority::policy::GrantStack,
    ) -> ral_core::carrier::IdentityTransport {
        let (spec, root) = test_attach();
        ral_core::carrier::IdentityTransport::boot(
            &crate::INSTALLERS,
            &ral_core::protocol::Attach::root(spec, root.with_grants(grants)),
        )
        .expect("the recipe boots a test engine")
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{engine_root, engine_runtime};
    use crate::app::SCRATCH_VAR;
    use ral_core::Shell;
    use ral_core::authority::grant::freeze::Anchor;
    use ral_core::authority::grant::value::{Axes, ExecTable, FsTable, GrantValue, Target};
    use ral_core::authority::policy::FsOp;
    use ral_core::environ::EnvVars;
    use ral_core::path::EnginePath;
    use ral_core::protocol::{EngineSpec, Root};

    /// The shell carries the scratch its root names, as a spelling, so a grant
    /// frozen in that session can anchor a `scratch` place to it: from the
    /// root's attach alone, so a `within [env: …]` cannot redirect it.
    #[test]
    fn a_shell_carries_the_scratch_its_root_names_whatever_the_overlay_says() {
        use ral_core::authority::grant::freeze::FreezeCtx;
        let scratch = std::env::temp_dir().join("a-session-scratch");
        let named = ral_core::path::utf8(&scratch).expect("a test path is Unicode");
        let spec = EngineSpec::new(
            crate::shell_eval::builtins::INSTALLER_TAG,
            EnginePath::Host(scratch.clone()),
        );
        let mut shell = Shell::root(
            engine_runtime(&spec)
                .expect("a test host resolves its baseline")
                .into(),
            ral_core::test_helper::process_cwd(),
        );
        let root = Root::new(EnginePath::Host(scratch.clone()))
            .with_env(EnvVars::from_pairs([(SCRATCH_VAR, named)]).unwrap());
        engine_root(&root, &mut shell).expect("the root names a scratch");
        assert_eq!(shell.scratch(), Some(named));

        shell.set_env_var(SCRATCH_VAR, "/elsewhere").unwrap();
        let frozen = FreezeCtx::at(
            shell.env_overrides().clone(),
            ral_core::path::CwdPath::new(shell.cwd()).expect("the shell's cwd is absolute"),
        )
        .with_scratch(shell.scratch())
        .place(&Anchor::Scratch.place("cache"))
        .expect("the place freezes")
        .expect("the place is live");
        assert!(frozen.as_str().contains("a-session-scratch"), "{frozen:?}");
        assert!(!frozen.as_str().contains("elsewhere"), "{frozen:?}");
    }

    /// A shell at `cwd`, as an engine roots one, and a root there naming the
    /// scratch and carrying `ceiling`.
    fn rooted(cwd: &std::path::Path, ceiling: GrantValue) -> (Shell, Root) {
        let at = EnginePath::Host(cwd.to_path_buf());
        let spec = EngineSpec::new(crate::shell_eval::builtins::INSTALLER_TAG, at.clone());
        let shell = Shell::root(
            engine_runtime(&spec)
                .expect("a test host resolves its baseline")
                .into(),
            ral_core::path::CwdPath::new(cwd.to_path_buf()).expect("an absolute cwd"),
        );
        let scratch = ral_core::path::utf8(cwd).expect("a test path is Unicode");
        let root = Root::new(at)
            .with_env(EnvVars::from_pairs([(SCRATCH_VAR, scratch)]).unwrap())
            .with_config(ceiling.into());
        (shell, root)
    }

    /// The root's ceiling is a grant value, resolved by the one front-end
    /// against the engine's own anchor: a `cwd` place names the root's cwd.
    #[test]
    fn a_ceiling_resolves_against_the_engines_own_anchor() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let tmp = ral_core::test_helper::real(dir.path())
            .as_path()
            .to_path_buf();
        let cwd = tmp.join("work");
        std::fs::create_dir(&cwd).expect("the root's cwd");
        let here = Axes {
            fs: Some(FsTable {
                read: vec![Anchor::Cwd.place(".")],
                ..FsTable::default()
            }),
            ..Axes::default()
        };
        let (mut shell, root) = rooted(&cwd, here.into());
        engine_root(&root, &mut shell).expect("the ceiling resolves");
        let reads = |path: &std::path::Path| {
            shell.admits_fs(FsOp::Read, &ral_core::test_helper::trace(path))
        };
        assert!(reads(&cwd.join("notes")), "the root's cwd is read");
        assert!(!reads(&tmp.join("elsewhere")), "nothing else is");
    }

    /// A full attach freezes home and command names in the root's effective
    /// environment, not the overlay the recipe starts with. No process env is
    /// mutated, and the program fixtures are resolved but never executed.
    #[cfg(unix)]
    #[test]
    fn a_full_attach_freezes_its_ceiling_in_the_roots_effective_environment() {
        use ral_core::authority::grant::value::LayerValue;
        use ral_core::carrier::{IdentityTransport, Transport as _};
        use ral_core::engine::EngineInstaller;
        use ral_core::error::Error;
        use ral_core::protocol::Attach;
        use ral_core::test_helper::Tree;
        use std::os::unix::fs::PermissionsExt;

        // Supply a deterministic initial overlay at the recipe boundary,
        // after core has seeded the host env, without mutating process state.
        fn root_with_initial_env(root: &Root, shell: &mut Shell) -> Result<(), Error> {
            let ral_core::path::EnginePath::Host(cwd) = &root.cwd else {
                panic!("the fixture attaches on the host");
            };
            for (name, dir) in [("HOME", "initial-home"), ("PATH", "initial-bin")] {
                let path = cwd.join(dir);
                shell
                    .set_env_var(name, ral_core::path::utf8(&path).expect("a Unicode path"))
                    .unwrap();
            }
            engine_root(root, shell)?;
            // Running reads its program. These files are outside both the
            // home read prefix and the scratch write prefix, so this observes
            // the frozen exec allowance without launching a command. The
            // outward grant value deliberately preserves a `name` spelling.
            for (bin, admitted) in [("root-bin", true), ("initial-bin", false)] {
                let program = cwd.join(bin).join("root-ceiling-tool");
                assert_eq!(
                    shell.admits_fs(FsOp::Read, &ral_core::test_helper::trace(program)),
                    admitted,
                    "only the root PATH's program is granted"
                );
            }
            Ok(())
        }
        static INSTALLERS: [EngineInstaller; 1] = [EngineInstaller {
            tag: crate::shell_eval::builtins::INSTALLER_TAG,
            runtime: engine_runtime,
            root: root_with_initial_env,
            narrow: crate::policy::base_layer,
        }];

        let tmp = tempfile::tempdir().expect("a temp dir");
        let tree = Tree::lay(
            tmp.path(),
            [
                "/initial-home/",
                "/root-home/",
                "/initial-bin/root-ceiling-tool",
                "/root-bin/root-ceiling-tool",
                "/scratch/",
            ],
        );
        for bin in ["initial-bin", "root-bin"] {
            std::fs::set_permissions(
                tree.path(&format!("/{bin}/root-ceiling-tool")),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("an executable fixture");
        }
        let ceiling: GrantValue = Axes {
            exec: Some(ExecTable {
                allow: vec![Target::Name("root-ceiling-tool".into())],
                ..ExecTable::default()
            }),
            fs: Some(FsTable {
                read: vec![Anchor::Home.place(".")],
                write: vec![Anchor::Scratch.place(".")],
                ..FsTable::default()
            }),
            ..Axes::default()
        }
        .into();
        let at = EnginePath::Host(tree.root().to_path_buf());
        let root = Root::new(at.clone())
            .with_env(
                EnvVars::from_pairs([
                    (SCRATCH_VAR, tree.at("/scratch")),
                    ("HOME", tree.at("/root-home")),
                    ("PATH", tree.at("/root-bin")),
                ])
                .unwrap(),
            )
            .with_config(ceiling.into());
        let transport = IdentityTransport::boot(
            &INSTALLERS,
            &Attach::root(
                EngineSpec::new(crate::shell_eval::builtins::INSTALLER_TAG, at),
                root,
            ),
        )
        .expect("the full root attach resolves its ceiling");

        assert_eq!(transport.env_var("HOME"), Ok(Some(tree.at("/root-home"))));
        assert_eq!(transport.env_var("PATH"), Ok(Some(tree.at("/root-bin"))));
        let expected: GrantValue = Axes {
            exec: Some(ExecTable {
                allow: vec![Target::Name("root-ceiling-tool".into())],
                ..ExecTable::default()
            }),
            fs: Some(FsTable {
                read: vec![Anchor::Absolute.place(tree.at("/root-home"))],
                write: vec![Anchor::Absolute.place(tree.at("/scratch"))],
                ..FsTable::default()
            }),
            ..Axes::default()
        }
        .into();
        assert_eq!(
            transport
                .grants()
                .expect("the ceiling probe answers")
                .layers,
            [LayerValue {
                grant: expected.into(),
                fragment: None,
            }],
            "neither the initial home nor its same-named program is granted"
        );
    }

    /// A ceiling the front-end refuses refuses the attach, in the decoder's
    /// words and with its hint: a file key naming a directory.
    #[test]
    fn a_ceiling_naming_a_directory_as_a_program_is_refused() {
        let tmp = tempfile::tempdir().expect("a temp dir");
        let dir = ral_core::path::utf8(tmp.path()).expect("a test path is Unicode");
        let program = Axes {
            exec: Some(ExecTable {
                allow: vec![Target::File(Anchor::Absolute.place(dir))],
                ..ExecTable::default()
            }),
            ..Axes::default()
        };
        let (mut shell, root) = rooted(tmp.path(), program.into());
        let Err(why) = engine_root(&root, &mut shell) else {
            panic!("a directory is no program");
        };
        assert!(why.describe().contains("is a directory"), "{why}");
        assert!(
            why.reason.advice().iter().any(|h| h.contains("`dir")),
            "{why:?}"
        );
        assert_eq!(why.hints(), []);
    }

    #[test]
    fn an_attach_naming_no_scratch_is_refused() {
        let temp = std::env::temp_dir();
        let spec = EngineSpec::new(
            crate::shell_eval::builtins::INSTALLER_TAG,
            EnginePath::Host(temp.clone()),
        );
        let mut shell = Shell::root(
            engine_runtime(&spec)
                .expect("a test host resolves its baseline")
                .into(),
            ral_core::test_helper::process_cwd(),
        );
        let Err(why) = engine_root(&Root::new(EnginePath::Host(temp)), &mut shell) else {
            panic!("an unnamed scratch must refuse the attach");
        };
        assert!(why.describe().contains(SCRATCH_VAR), "{why}");
    }
}
