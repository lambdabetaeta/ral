//! Full-screen TUI frontend.
//!
//! One [`Sink`] implementation, plus the REPL loop the user types into.
//! The TUI owns raw-mode, bracketed-paste, the alternate screen, and
//! mouse capture through [`TerminalGuard`]; the agent core in
//! [`crate::bus`] and [`crate::agent`] sees only an
//! [`crate::bus::Emitter`] / [`Event`] channel.
//!
//! The app owns its scrollback rather than delegating it to the host
//! terminal: each session is a buffer of collapsible [`block`]s and the
//! whole frame is redrawn every tick.  A tool call shows its summary and
//! opens to the full ral script on a click; the wheel scrolls, click-drag
//! selects and copies, and Shift-drag falls through to the terminal'\''s own
//! selection.  Assistant text accumulates into the active [`Viewport`]'\''s
//! paragraph buffer and commits one fence-safe paragraph at a time — no
//! live preview row.
mod app;
mod banner;
mod block;
mod commands;
mod fidelity;
mod gesture;
mod group;
mod highlight;
mod line;
mod matrix;
mod md;
mod model_picker;
mod palette;
mod picker;
mod prompt;
mod rail;
mod render;
mod select;
mod status;
mod surface;
mod tabs;
mod terminal;
mod tui_loop;
mod viewport;

use std::time::Duration;

pub(super) use app::App;
pub use banner::SessionInfo;
pub use tui_loop::run;

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

pub(super) const PROMPT_PAD_H: u16 = 1;

/// Left gutter for the transcript, queued-prompt strip, and rule line.
/// Gives the marginal rail breathing room from the terminal edge so it
/// reads as a Bertin data column rather than frame chrome.
pub(super) const LEFT_MARGIN: u16 = 2;
/// Minimum useful width of the pinned-state register column, in columns.  Once
/// the content area has this much space to the right of the `READ_W`-capped
/// transcript, the register takes all of it.
pub(super) const REGISTER_MIN_W: u16 = 35;
/// How long a subagent tab stays in the rotation after the session
/// dies — long enough for the user to tab over and inspect the final
/// frame of its scrollback, short enough not to clutter the tab bar.
pub(super) const LINGER: Duration = Duration::from_secs(90);
/// Display label for the root session in the tab bar.
pub(super) const ROOT_TITLE: &str = "main";

/// Braille spinner glyphs for the terminal tab title, rotated 4 ticks per frame (~15 fps).
pub(super) const SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];
