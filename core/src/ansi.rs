// ── ANSI styling ──────────────────────────────────────────────────────────
//
// Single source of truth for ANSI escape constants, color-gating predicates,
// and the configurable OutputTheme used for REPL value output.
//
// Gating helpers (`use_color`, `use_ui_color`) consult a cached TerminalState
// seeded once at REPL startup via `set_terminal`.  Batch runs and
// early-startup errors fall back to inline probing.

use std::sync::{LazyLock, OnceLock, RwLock};

use crate::io::TerminalState;

// ── Constants ─────────────────────────────────────────────────────────────

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const REVERSE: &str = "\x1b[7m";

pub const RED: &str = "\x1b[0;31m";
pub const GREEN: &str = "\x1b[0;32m";
pub const YELLOW: &str = "\x1b[0;33m";
pub const BLUE: &str = "\x1b[0;34m";
pub const MAGENTA: &str = "\x1b[0;35m";
pub const CYAN: &str = "\x1b[0;36m";
pub const WHITE: &str = "\x1b[0;37m";

pub const BOLD_RED: &str = "\x1b[1;31m";
pub const BOLD_GREEN: &str = "\x1b[1;32m";
pub const BOLD_YELLOW: &str = "\x1b[1;33m";
pub const BOLD_BLUE: &str = "\x1b[1;34m";
pub const BOLD_MAGENTA: &str = "\x1b[1;35m";
pub const BOLD_CYAN: &str = "\x1b[1;36m";

pub const UNDERLINE_RED: &str = "\x1b[4;31m";

// ── Color-gating ──────────────────────────────────────────────────────────

static CACHED_TERMINAL: OnceLock<TerminalState> = OnceLock::new();

/// Seed the cached `TerminalState` consulted by `use_color` and `use_ui_color`.
/// Call once per process after probing.  Subsequent calls are silently ignored.
pub fn set_terminal(t: &TerminalState) {
    let _ = CACHED_TERMINAL.set(*t);
}

/// Whether to emit ANSI color on stderr (diagnostics, errors, warnings).
///
/// Consults the cached `TerminalState` when available so all ANSI gating
/// agrees on one source of truth.  Falls back to inline probing for batch
/// runs and early-startup errors.
pub fn use_color() -> bool {
    if let Some(t) = CACHED_TERMINAL.get() {
        return t.stderr_ansi_ok();
    }
    if anstyle_query::no_color() {
        return false;
    }
    if !anstyle_query::term_supports_ansi_color() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    }
    #[cfg(windows)]
    {
        crate::compat::is_console(crate::compat::STD_ERROR_HANDLE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Whether to emit ANSI color on stdout (REPL value output, help, etc.).
///
/// Checks `ui_ansi_ok()` — stdout tty + TERM + NO_COLOR — rather than the
/// stderr-oriented `stderr_ansi_ok()` used by `use_color`.
pub fn use_ui_color() -> bool {
    CACHED_TERMINAL.get().is_some_and(|t| t.ui_ansi_ok())
}

/// Return `code` when `enabled` is true, otherwise the empty string.
///
/// Convenience for the common `if color { "\x1b[...]" } else { "" }` pattern.
pub fn when(enabled: bool, code: &'static str) -> &'static str {
    if enabled { code } else { "" }
}

// ── OutputTheme ───────────────────────────────────────────────────────────

/// Styling applied to ral-computed values printed at the REPL prompt.
///
/// The default theme prefixes each value with `"=> "` in yellow.  Both fields
/// are configurable via the `theme` key in the RC file.  Color is suppressed
/// automatically when `use_ui_color()` returns false.
#[derive(Clone, Debug)]
pub struct OutputTheme {
    /// String prepended to every printed value.  Default: `"=> "`.
    pub value_prefix: String,
    /// ANSI SGR escape for value output.  `None` suppresses color entirely.
    pub value_color: Option<String>,
}

impl Default for OutputTheme {
    fn default() -> Self {
        Self {
            value_prefix: "=> ".into(),
            value_color: Some(YELLOW.into()),
        }
    }
}

static OUTPUT_THEME: LazyLock<RwLock<OutputTheme>> =
    LazyLock::new(|| RwLock::new(OutputTheme::default()));

/// Replace the active output theme.  Called once after the RC file is loaded.
pub fn set_output_theme(theme: OutputTheme) {
    if let Ok(mut g) = OUTPUT_THEME.write() {
        *g = theme;
    }
}

/// Return a snapshot of the current output theme.
pub fn output_theme() -> OutputTheme {
    OUTPUT_THEME.read().map(|g| g.clone()).unwrap_or_default()
}

/// Map a standard color name to its ANSI SGR escape string.
///
/// Accepts `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
/// `white`, and `none`/`off` (which returns `None` to suppress color).
/// Unrecognised names also return `None`.
pub fn named_color(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "none" | "off" => None,
        "black" => Some("\x1b[0;30m".into()),
        "red" => Some(RED.into()),
        "green" => Some(GREEN.into()),
        "yellow" => Some(YELLOW.into()),
        "blue" => Some(BLUE.into()),
        "magenta" => Some(MAGENTA.into()),
        "cyan" => Some(CYAN.into()),
        "white" => Some(WHITE.into()),
        _ => None,
    }
}
