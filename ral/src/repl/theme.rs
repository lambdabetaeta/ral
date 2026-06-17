//! REPL value-output styling.
//!
//! `OutputTheme` controls how the REPL renders the result of a top-level
//! expression — a `value_prefix` string (default `"=> "`) and an optional
//! `value_color` (an ANSI SGR escape).  Both fields are configurable from
//! the RC file's `theme` key.
//!
//! Color is suppressed automatically when [`ral_core::ansi::use_ui_color`]
//! returns false, so the theme can store an unconditional `Some(color)`.
//!
//! The theme is process-global state — there is exactly one REPL per
//! process and it consults the theme from the value-printing path
//! (`repl::exec::print_result`).  Stored behind a `RwLock` so the RC
//! file can replace it once during startup without imposing locking on
//! the read path beyond a snapshot clone.

use ral_core::ansi::{BLACK, BLUE, CYAN, GREEN, MAGENTA, RED, WHITE, YELLOW};
use std::sync::{LazyLock, RwLock};

/// Styling applied to ral-computed values printed at the REPL prompt.
#[derive(Clone, Debug)]
pub(crate) struct OutputTheme {
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
pub(crate) fn set_output_theme(theme: OutputTheme) {
    if let Ok(mut g) = OUTPUT_THEME.write() {
        *g = theme;
    }
}

/// Return a snapshot of the current output theme.
pub(crate) fn output_theme() -> OutputTheme {
    OUTPUT_THEME.read().map(|g| g.clone()).unwrap_or_default()
}

/// Map a standard color name to its ANSI SGR escape string.
///
/// Accepts `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
/// `white`, and `none`/`off` (which returns `None` to suppress color).
/// Unrecognised names also return `None`.
pub(crate) fn named_color(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "none" | "off" => None,
        "black" => Some(BLACK.into()),
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
