//! Full-screen TUI frontend.
//!
//! One [`crate::bus::Sink`] implementation, plus the REPL loop the user
//! types into.  The TUI owns raw-mode, bracketed-paste, the alternate
//! screen, and mouse capture through [`terminal::TerminalGuard`]; the agent
//! core in [`crate::bus`] and [`crate::agent`] sees only a
//! [`crate::bus::Emitter`] channel.
//!
//! The app owns its scrollback rather than delegating it to the host
//! terminal: each session is a buffer of collapsible [`block`]s and the
//! whole frame is redrawn every tick.  A tool call shows its summary and
//! opens to the full ral script on a click; the wheel scrolls, click-drag
//! selects and copies, and Shift-drag falls through to the terminal's own
//! selection.  Assistant text accumulates into the active [`viewport::Viewport`]'s
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
mod login;
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

/// How long a subagent tab stays in the rotation after the session
/// dies — long enough for the user to tab over and inspect the final
/// frame of its scrollback, short enough not to clutter the tab bar.
pub(super) const LINGER: Duration = Duration::from_secs(90);
/// Display label for the root session in the tab bar.
pub(super) const ROOT_NAME: &str = "main";
