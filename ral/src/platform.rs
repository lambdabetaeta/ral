//! Shared session bring-up helpers: terminal probing, user directories,
//! exit-code hints, and `--capabilities` application.
//!
//! Centralised here so the batch runner and the `repl/` submodules can reach
//! them through `crate::platform::*` without one path reaching into the
//! other's module.  Both Unix and Windows env-var fallbacks are encoded
//! explicitly.

use ral_core::exit_hints::ExitHints;
use ral_core::io::{InteractiveMode, TerminalState};
use ral_core::types::{Break, Escape};
use ral_core::{Shell, diagnostic};
use std::process::ExitCode;

/// Probe the terminal under the active `RAL_INTERACTIVE_MODE`, plumb
/// it into the diagnostic subsystem, and return both halves.  When
/// `warn` is set, an unrecognised mode value emits a shell warning;
/// callers in non-interactive modes pass `false` to stay quiet.
pub(crate) fn probe_terminal(warn: bool) -> (InteractiveMode, TerminalState) {
    let (mode, terminal, mode_warn) = TerminalState::probe_from_env();
    if warn && let Some(msg) = mode_warn {
        diagnostic::shell_warning(&msg);
    }
    diagnostic::set_terminal(&terminal);
    (mode, terminal)
}

/// Home directory, deferring the resolution rule to
/// [`ral_core::path::home_from_env_or_dot`].
pub(crate) fn home_dir() -> String {
    ral_core::path::home_from_env_or_dot()
}

static DEFAULT_EXIT_HINTS: &str = include_str!("../../data/exit-hints.txt");

/// Load exit-code hints: user override in data dir, else the embedded default.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:exit-hints-read] startup read of the optional exit-hints override file; not turn-time model I/O"
)]
pub(crate) fn load_exit_hints() -> ExitHints {
    let text = ral_core::path::config::xdg_data_subpath("ral/exit-hints.txt")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let text = if text.is_empty() {
        DEFAULT_EXIT_HINTS
    } else {
        &text
    };
    ExitHints::from_text(text)
}

/// Clamp a shell exit code to the `0..=255` process byte-status range.
///
/// Ral carries exit codes as `i32` (an `exit` argument, a signal-derived
/// status, a cancel cause); the OS process status is a single byte. Every
/// mode — batch, `-c`, the REPL turn loop — funnels its final code through
/// here so the clamp-and-narrow lives in one place.
pub(crate) fn exit_byte(code: i32) -> u8 {
    #[allow(
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255, a byte exit status"
    )]
    let byte = code.clamp(0, 255) as u8;
    byte
}

/// Apply the `--capabilities` profiles as a session-wide ceiling, mapping the
/// outcome to a process exit.
///
/// The composition mechanism (load, `meet`-fold, freeze, push) lives in
/// [`ral_core::capability::apply_session_profiles`]. A load failure is
/// attributed to the flag that supplied the profiles and yields exit 2; an
/// escape raised while a profile evaluates (`exit`, a stopped child) propagates
/// to the same process exit it would from any other script.
pub(crate) fn apply_session_capabilities(
    shell: &mut Shell,
    paths: &[std::path::PathBuf],
) -> Result<(), ExitCode> {
    match ral_core::capability::apply_session_profiles(shell, paths) {
        Ok(()) => Ok(()),
        Err(Break::Error(e)) => {
            diagnostic::cmd_error("ral", &format!("--capabilities: {}", e.message));
            Err(ExitCode::from(2))
        }
        Err(Break::Escape(Escape::Exit(code))) => Err(ExitCode::from(exit_byte(code))),
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => Err(ExitCode::from(1)),
    }
}
