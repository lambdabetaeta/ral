use ral_core::ansi::{self, BOLD_CYAN, BOLD_RED, BOLD_YELLOW, RESET};
use ral_core::types::Error;

/// Format a plugin error as one or more lines (no trailing newline).
///
/// Renders `plugin '<name>': <context>: <message>`, followed by a hint line
/// if present.  Uses bold-red styling when colour is enabled.  Returned as
/// a single `String` so callers may either print immediately or buffer it
/// for deferred display past terminal escapes that would clobber it.
pub(super) fn format_plugin_error(plugin_name: &str, context: &str, err: &Error) -> String {
    let c = ansi::use_color();
    let (red, cyan, reset) = (
        ansi::when(c, BOLD_RED),
        ansi::when(c, BOLD_CYAN),
        ansi::when(c, RESET),
    );
    let mut s = format!(
        "{red}plugin{reset} '{plugin_name}': {context}: {}",
        err.message
    );
    if let Some(hint) = err.hint.as_deref() {
        use std::fmt::Write;
        let _ = write!(s, "\n  {cyan}hint{reset}: {hint}");
    }
    s
}

/// Print a plugin error to stderr with consistent formatting.
///
/// Use only from contexts where no readline-driven escape sequence is about
/// to clobber the line (e.g. lifecycle hooks at startup).  Inside the
/// readline loop, defer via [`super::plugin::defer_plugin_error`] instead.
pub(super) fn plugin_error(plugin_name: &str, context: &str, err: &Error) {
    eprintln!("{}", format_plugin_error(plugin_name, context, err));
}

/// Print a plugin warning to stderr with consistent formatting.
pub(super) fn plugin_warning(plugin_name: &str, msg: &str) {
    let c = ansi::use_color();
    let (yellow, reset) = (ansi::when(c, BOLD_YELLOW), ansi::when(c, RESET));
    eprintln!("{yellow}plugin{reset} '{plugin_name}': warning: {msg}");
}

pub(super) fn format_repl_parse_error(message: &str) -> String {
    let c = ansi::use_color();
    format!(
        "{}error{}: {message} (exit status 2)\n",
        ansi::when(c, BOLD_RED),
        ansi::when(c, RESET),
    )
}

pub(super) fn should_use_compact_parse_error(trimmed: &str, message: &str) -> bool {
    !trimmed.contains('\n')
        && !trimmed.contains(';')
        && message.contains("value cannot appear in command position")
}
