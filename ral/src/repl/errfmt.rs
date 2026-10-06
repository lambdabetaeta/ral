//! REPL-specific error formatting.
//!
//! Plugin diagnostics live here.  The full ariadne-rendered errors come
//! from `Error::render`; these helpers handle the shorter,
//! REPL-styled notices — disable and warning — and the line that says what
//! a failed load left out.

use ral_core::ansi::{self, BOLD_YELLOW, RESET};
use ral_core::types::Error;
use ral_core::{Shell, terminal};
use ral_core::{err, errln};

/// Print a failed load's report, then `ral: {what}: …` saying how far it got:
/// skipped whole if it never compiled, which a compile report cannot say, or
/// else `ran`, for a caller with that to say.
pub(super) fn report_failed_load(shell: &Shell, what: &str, e: &Error, ran: Option<&str>) {
    err!("{}", e.render(shell.sources(), None));
    let why = match e.rejection {
        Some(_) => Some("skipped, since it does not compile"),
        None => ran,
    };
    if let Some(why) = why {
        terminal::cmd_error("ral", &format!("{what}: {why}"));
    }
}

/// Format the circuit-breaker's disable notice: `plugin '<name>': hook
/// '<kind>' disabled for this session (<reason>)`.  Returned as a string (no
/// trailing newline) so the readline loop can defer it past line-erase
/// escapes alongside the other plugin diagnostics.
pub(super) fn format_plugin_disabled(plugin_name: &str, kind: &str, reason: &str) -> String {
    let c = terminal::stderr_color();
    let (yellow, reset) = (ansi::when(c, BOLD_YELLOW), ansi::when(c, RESET));
    format!(
        "{yellow}plugin{reset} '{plugin_name}': hook '{kind}' disabled for this session ({reason})"
    )
}

/// Print a plugin warning to stderr with consistent formatting.
pub(super) fn plugin_warning(plugin_name: &str, msg: &str) {
    let c = terminal::stderr_color();
    let (yellow, reset) = (ansi::when(c, BOLD_YELLOW), ansi::when(c, RESET));
    errln!("{yellow}plugin{reset} '{plugin_name}': warning: {msg}");
}
