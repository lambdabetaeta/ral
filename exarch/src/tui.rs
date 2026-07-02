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
/// Width of the pinned-state register column, in columns — a framed gauge
/// (`│ tasks ▓▓░ 3/8 │`) plus its borders and a padding column each side.
pub(super) const REGISTER_W: u16 = 35;
/// Minimum reading gap between the `READ_W`-capped transcript and the register
/// column.  The register is reserved only when the content area is at least
/// `LEFT_MARGIN + READ_W + REGISTER_GAP + REGISTER_W` wide — wide enough that
/// reclaiming the dead right margin costs the transcript nothing; below that it
pub(super) const REGISTER_GAP: u16 = 4;
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
