//! The full-screen TUI frontend.
//!
//! [`enter`] takes the terminal — raw mode, alternate screen, bracketed paste,
//! mouse capture — [`choose`] picks a model on it when a launch has none to
//! restore, and [`run`] holds it for the REPL loop the user types into.
//!
//! The agent core sees only a [`crate::bus::Emitter`] channel. The frontend
//! is deliberately not a [`crate::bus::Sink`]: it drains the same bus on its
//! render cadence instead, and owns its scrollback rather than the host
//! terminal's, redrawing the whole screen each tick.

mod app;
mod banner;
mod block;
mod commands;
mod dial;
mod diff;
mod export;
mod fidelity;
mod gauge;
mod gesture;
mod group;
mod highlight;
mod line;
mod login;
mod matrix;
mod md;
mod mirror;
mod model_picker;
mod overlay;
mod palette;
mod paste_burst;
mod picker;
mod prompt;
mod providers;
mod rail;
mod render;
mod resources;
mod rewind;
mod row;
mod scrollback;
mod select;
mod status;
mod tabs;
mod terminal;
mod text;
mod tui_loop;

use ral_core::own::OwnDir;
use std::time::Duration;

pub(super) use app::App;
pub use export::{Format, export, export_to};
pub use model_picker::choose;
pub use picker::Pick;
pub use terminal::TerminalGuard;
pub use tui_loop::run;

/// Take the terminal for the TUI — raw mode, the alternate screen, and fd 2
/// redirected to `run`'s `stderr.log` — before the first thing it shows.
///
/// # Errors
/// If the terminal cannot be put into those modes, or the log not opened.
pub fn enter(run: &OwnDir) -> Result<TerminalGuard, String> {
    TerminalGuard::enter(run).map_err(|e| format!("ratatui init: {e}"))
}

/// How long a dead subagent's tab survives in the bar, so its last frame is
/// still readable before `tabs` ages it out.
pub(super) const LINGER: Duration = Duration::from_secs(90);
/// How long a resting helper may sit idle before its row demotes to a
/// compact slate, in place in the spawn tree.
///
/// A frontend rule read off the agent's own exchange clock: nothing agent-side
/// acts at the mark.
pub(crate) const DEMOTE_IDLE: Duration = Duration::from_mins(5);
