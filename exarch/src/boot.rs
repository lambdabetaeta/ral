//! Session boot: the shell every exarch engine starts from.
//! Nothing here runs per exchange — per-session disk state is
//! [`crate::record::AgentLog`].

use crate::app::{SCRATCH_VAR, seed_var};
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::authority::policy::{Capabilities, GrantLayer};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::host::EnvVars;
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
pub fn engine_runtime(spec: &EngineSpec) -> Result<Runtime, String> {
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

/// Exarch's root-only recipe: the agent library, hints, the detach budget,
/// and the root's env seeded as bindings before the ledgers arm.
///
/// The scratch is the host's, named by the root, and the shell carries its
/// spelling, so that a `scratch` place in any grant frozen in this session
/// anchors to it, on the host and in a guest alike.  The root's attach is
/// authored by the host and is where it is read from, never the overlay, which
/// a `within [env: …]` could redirect.
///
/// A root whose config is not unit carries the ceiling its host authored for
/// this engine, resolved here, on the host it runs on, and pushed as a session
/// layer: synod's guest names its places in the guest's namespace.
///
/// # Errors
/// A root that names no scratch, or whose ceiling does not decode or resolve.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn engine_root(root: &Root, shell: &mut Shell) -> Result<(), String> {
    let Some((_, scratch)) = root.env.iter().find(|(name, _)| name == SCRATCH_VAR) else {
        return Err(format!(
            "exarch engine: the root attach names no ${SCRATCH_VAR}: the host owns the scratch and names it in every root attach"
        ));
    };
    shell.set_scratch(scratch.as_str());
    if !matches!(root.config, FOValue::Unit) {
        let ceiling = Capabilities::decode(&root.config)
            .map_err(|e| format!("exarch engine: the root's ceiling does not decode: {e}"))?;
        let layer = GrantLayer::from_capabilities(ceiling, &EnvVars::new()).map_err(|e| {
            format!(
                "exarch engine: the root's ceiling cannot be resolved: {}",
                e.message
            )
        })?;
        shell.push_session_capabilities(layer);
    }
    crate::library::install_agent_library(&ral_core::shell::runtime::Mooring::adrift(), shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(shell);
    shell.set_exit_hints(ral_core::shell::runtime::ExitHints::from_text(
        include_str!("../../data/exit-hints.txt"),
    ));
    #[cfg(unix)]
    shell.arm_detach(shell_eval::DETACH_BIRTH_BUDGET);
    for (name, value) in &root.env {
        seed_var(shell, name, value);
    }
    shell_eval::arm_session_ledgers(shell);
    Ok(())
}

/// Suppress ANSI colour at the source.  A tool call's stdout is a pipe, never a
/// TTY, so these only have to beat config that *forces* colour;
/// [`crate::agent::digest`]'s strip covers the tools that honour neither.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    shell.set_env_var("NO_COLOR", "1");
    shell.set_env_var("CLICOLOR_FORCE", "0");
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
    use ral_core::protocol::{EngineSpec, Root};

    /// The engine and root a test attaches, the root naming the scratch.
    pub(crate) fn test_attach() -> (EngineSpec, Root) {
        let temp = std::env::temp_dir();
        (
            EngineSpec::new(
                builtins::INSTALLER_TAG,
                ral_core::path::EnginePath::Host(temp.clone()),
            ),
            Root::new(ral_core::path::EnginePath::Host(temp.clone())).with_env(vec![(
                SCRATCH_VAR.to_owned(),
                ral_core::path::utf8(&temp)
                    .expect("a temp dir is Unicode")
                    .to_owned(),
            )]),
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
    use super::{engine_root, engine_runtime, test_shell};
    use crate::app::{EXARCH, SCRATCH_VAR, Scratch};
    use ral_core::Shell;
    use ral_core::path::EnginePath;
    use ral_core::protocol::{EngineSpec, Root};

    /// Cargo's own TLS wants a Mach service no profile admits, so a confined session
    /// must hand it the `git` binary instead — invisible in every log, hence
    /// asserted here.
    #[test]
    fn a_confined_session_tells_cargo_to_fetch_through_git() {
        let scratch = Scratch::for_test(EXARCH, "confined-settings").expect("test scratch");
        let mut shell = test_shell();
        scratch.install_into(&mut shell);
        assert_eq!(
            shell.env_var("CARGO_NET_GIT_FETCH_WITH_CLI"),
            Some("true".to_string())
        );
    }

    /// The shell carries the scratch its root names, as a spelling, so a grant
    /// frozen in that session can anchor a `scratch` place to it: from the
    /// root's attach alone, so a `within [env: …]` cannot redirect it.
    #[test]
    fn a_shell_carries_the_scratch_its_root_names_whatever_the_overlay_says() {
        use ral_core::authority::grant::freeze::{FreezeCtx, Place};
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
            .with_env(vec![(SCRATCH_VAR.to_owned(), named.to_owned())]);
        engine_root(&root, &mut shell).expect("the root names a scratch");
        assert_eq!(shell.scratch(), Some(named));

        shell.set_env_var(SCRATCH_VAR, "/elsewhere");
        let frozen = FreezeCtx::at(
            shell.env_overrides().clone(),
            ral_core::path::CwdPath::new(shell.cwd()).expect("the shell's cwd is absolute"),
        )
        .with_scratch(shell.scratch())
        .place(&Place::Scratch("cache".into()))
        .expect("the place freezes")
        .expect("the place is live");
        assert!(frozen.as_str().contains("a-session-scratch"), "{frozen:?}");
        assert!(!frozen.as_str().contains("elsewhere"), "{frozen:?}");
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
        assert!(why.contains(SCRATCH_VAR), "{why}");
    }
}
