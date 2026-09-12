//! REPL-specific error formatting.
//!
//! Plugin diagnostics live here.  The full ariadne-rendered errors come
//! from `ral_core::diagnostic`; these helpers handle the shorter,
//! REPL-styled notices — disable and warning, not error.

use ral_core::ansi::{self, BOLD_YELLOW, RESET};

/// Format the circuit-breaker's disable notice: `plugin '<name>': hook
/// '<kind>' disabled for this session (<reason>)`.  Returned as a string (no
/// trailing newline) so the readline loop can defer it past line-erase
/// escapes alongside the other plugin diagnostics.
pub(super) fn format_plugin_disabled(plugin_name: &str, kind: &str, reason: &str) -> String {
    let c = ansi::use_color();
    let (yellow, reset) = (ansi::when(c, BOLD_YELLOW), ansi::when(c, RESET));
    format!(
        "{yellow}plugin{reset} '{plugin_name}': hook '{kind}' disabled for this session ({reason})"
    )
}

/// Print a plugin warning to stderr with consistent formatting.
pub(super) fn plugin_warning(plugin_name: &str, msg: &str) {
    let c = ansi::use_color();
    let (yellow, reset) = (ansi::when(c, BOLD_YELLOW), ansi::when(c, RESET));
    eprintln!("{yellow}plugin{reset} '{plugin_name}': warning: {msg}");
}
