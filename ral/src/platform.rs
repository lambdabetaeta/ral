//! Cross-platform helpers for user directories and environment seeding.
//!
//! Centralised here so `main.rs` and the `repl/` submodules can reach them
//! through `crate::platform::*` without chaining `super::super::`.  Both
//! Unix and Windows env-var fallbacks are encoded explicitly.

use ral_core::diagnostic;
use ral_core::exit_hints::ExitHints;
use ral_core::io::{InteractiveMode, TerminalState};

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

/// Home directory: `$HOME` on Unix, `%USERPROFILE%` on Windows.
/// Falls back to `.` so REPL completion never panics on a path
/// join.  Routes through [`ral_core::path::home_from_env_or_dot`]
/// so the resolution rule lives in one place.
pub(crate) fn home_dir() -> String {
    ral_core::path::home_from_env_or_dot()
}

/// Current user name: `$USER` on Unix, `%USERNAME%` on Windows.
/// Routes through [`ral_core::path::user_name_from_env`] so the
/// resolution rule lives in one place.
pub(crate) fn user_name() -> String {
    ral_core::path::user_name_from_env()
}

static DEFAULT_EXIT_HINTS: &str = include_str!("../../data/exit-hints.txt");

/// Load exit-code hints: user override in data dir, else the embedded default.
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
