//! Session boot: the shell every exarch seat starts from.
//! Nothing here runs per exchange — per-session disk state is
//! [`crate::record::AgentLog`].

use crate::app::{EXARCH, Scratch, seed_var};
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::Shell;
use ral_core::terminal::TerminalState;

/// The process-level signal ceremony, once at each entry point that hosts an
/// identity engine: exarch's cancel chain layers over ral's handlers.
pub fn face_process_signals(terminal: &TerminalState) {
    ral_core::process::clear();
    ral_core::process::install_handlers();
    crate::signals::install();
    terminal.seat();
}

/// Exarch's one `EngineInstaller::boot`, under either carrier.
///
/// The dressed shell, `detach` where the double fork exists, and the Attach's
/// env seeded as bindings before the ledgers arm. A scratch the Attach names
/// is the host's; with none named — a guest engine — it mints its own, kept
/// as long as the engine.
///
/// # Errors
/// A scratch that could not be created, which refuses the attach.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn engine_boot_shell(
    attach: &ral_core::protocol::Attach,
) -> Result<ral_core::engine::Booted, String> {
    let mut shell = ral_core::boot::boot_shell(
        attach.terminal,
        &shell_eval::PRELUDE,
        &builtins::host_surface(),
    );
    crate::library::install_agent_library(&ral_core::types::Mooring::adrift(), &mut shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(&mut shell);
    shell.set_exit_hints(ral_core::types::ExitHints::from_text(include_str!(
        "../../data/exit-hints.txt"
    )));
    #[cfg(unix)]
    {
        shell.install_builtins(ral_core::builtins::DETACH_BUILTIN);
        shell.arm_detach(shell_eval::DETACH_BIRTH_BUDGET);
    }
    let named = attach
        .env
        .iter()
        .any(|(name, _)| *name == EXARCH.scratch_var());
    let keep: Box<dyn Send> = if named {
        Box::new(())
    } else {
        let scratch = Scratch::new(EXARCH)
            .map_err(|e| format!("exarch engine: could not create its scratch directory: {e}"))?;
        scratch.install_into(&mut shell);
        Box::new(scratch)
    };
    for (name, value) in &attach.env {
        seed_var(&mut shell, name, value);
    }
    shell_eval::arm_session_ledgers(&mut shell);
    Ok(ral_core::engine::Booted { shell, keep })
}

/// An Attach for a test engine, naming a scratch so the recipe mints none.
#[cfg(test)]
pub(crate) fn test_attach() -> ral_core::protocol::Attach {
    let temp = std::env::temp_dir();
    let mut attach =
        ral_core::protocol::Attach::new(builtins::INSTALLER_TAG, temp.clone(), temp.clone());
    attach
        .env
        .push((EXARCH.scratch_var(), temp.to_string_lossy().into_owned()));
    attach
}

/// A dressed shell for a test, over no scratch of its own.
#[cfg(test)]
pub(crate) fn test_shell() -> Shell {
    engine_boot_shell(&test_attach())
        .expect("a named scratch boots without minting one")
        .shell
}

/// An identity engine for a test, booted through the one recipe.
#[cfg(test)]
pub(crate) fn test_transport() -> ral_core::carrier::IdentityTransport {
    ral_core::carrier::IdentityTransport::boot(&crate::INSTALLERS, &test_attach())
        .expect("the recipe boots a test engine")
}

/// Suppress ANSI colour at the source.  A tool call's stdout is a pipe, never a
/// TTY, so these only have to beat config that *forces* colour;
/// [`crate::agent::digest`]'s strip covers the tools that honour neither.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    shell.set_env_var("NO_COLOR", "1");
    shell.set_env_var("CLICOLOR_FORCE", "0");
}

#[cfg(test)]
mod tests {
    use super::test_shell;
    use crate::app::{EXARCH, Scratch};

    /// Cargo's own TLS wants a door no profile admits, so a confined session
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
}
