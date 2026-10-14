//! Session boot: the shell every exarch seat starts from.
//! Nothing here runs per exchange — per-session disk state is
//! [`crate::record::AgentLog`].

use crate::app::{EXARCH, seed_var};
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::boot::boot_runtime;
use ral_core::protocol::Attach;
use ral_core::terminal::TerminalState;
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
/// None: the signature is the installer's.
pub fn engine_runtime(attach: &Attach) -> Result<Runtime, String> {
    let mut runtime = boot_runtime(
        attach.terminal,
        &shell_eval::PRELUDE,
        &builtins::host_surface(),
    );
    #[cfg(unix)]
    runtime.install_builtins(ral_core::builtins::DETACH_BUILTIN);
    runtime.install_library_docs(crate::library::agent_library_docs());
    Ok(runtime)
}

/// Exarch's root-only recipe: the agent library, hints, the detach budget,
/// and the Attach's env seeded as bindings before the ledgers arm. The
/// scratch is the host's, named by the Attach.
///
/// # Errors
/// An Attach that names no scratch.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn engine_root(attach: &Attach, shell: &mut Shell) -> Result<(), String> {
    let var = EXARCH.scratch_var();
    if !attach.env.iter().any(|(name, _)| *name == var) {
        return Err(format!(
            "exarch engine: the attach names no ${var}: the host owns the scratch and names it in every attach"
        ));
    }
    crate::library::install_agent_library(&ral_core::types::Mooring::adrift(), shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(shell);
    shell.set_exit_hints(ral_core::types::ExitHints::from_text(include_str!(
        "../../data/exit-hints.txt"
    )));
    #[cfg(unix)]
    shell.arm_detach(shell_eval::DETACH_BIRTH_BUDGET);
    for (name, value) in &attach.env {
        seed_var(shell, name, value);
    }
    shell_eval::arm_session_ledgers(shell);
    Ok(())
}

/// An Attach for a test engine, naming the scratch.
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
    let attach = test_attach();
    let mut shell = Shell::root(engine_runtime(&attach).expect("exarch's runtime derives"));
    ral_core::boot::seed_host_env(&mut shell);
    engine_root(&attach, &mut shell).expect("test_attach names the scratch");
    shell
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
    use super::{engine_root, engine_runtime, test_shell};
    use crate::app::{EXARCH, Scratch};
    use ral_core::Shell;

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

    /// Pins the rule that makes a child engine's runtime its parent's.
    #[test]
    fn a_root_installs_no_builtins_the_runtime_lacks() {
        let names = |shell: &Shell| {
            let mut names: Vec<String> = shell.builtin_names().map(str::to_owned).collect();
            names.sort();
            names
        };
        let runtime = engine_runtime(&super::test_attach()).expect("exarch's runtime derives");
        assert_eq!(names(&test_shell()), names(&Shell::root(runtime)));
    }

    #[test]
    fn an_attach_naming_no_scratch_is_refused() {
        let temp = std::env::temp_dir();
        let attach = ral_core::protocol::Attach::new(
            crate::shell_eval::builtins::INSTALLER_TAG,
            temp.clone(),
            temp,
        );
        let mut shell = Shell::root(engine_runtime(&attach).expect("exarch's runtime derives"));
        let Err(why) = engine_root(&attach, &mut shell) else {
            panic!("an unnamed scratch must refuse the attach");
        };
        assert!(why.contains(&EXARCH.scratch_var()), "{why}");
    }
}
