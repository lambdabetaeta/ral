//! Full-screen TUI frontend: [`run`] holds the terminal — raw mode, alternate
//! screen, bracketed paste, mouse capture — and the REPL loop the user types
//! into.
//!
//! The agent core sees only a [`crate::bus::Emitter`] channel. The frontend
//! is deliberately not a [`crate::bus::Sink`]: it drains the same bus on its
//! render cadence instead, and owns its scrollback rather than the host
//! terminal's, redrawing the whole screen each tick.

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
mod row;
mod select;
mod status;
mod tabs;
mod terminal;
mod tui_loop;
mod viewport;

use std::time::Duration;

pub(super) use app::App;
pub use banner::SessionInfo;
pub use tui_loop::run;

/// How long a dead subagent's tab survives in the bar, so its last frame is
/// still readable before `tabs` ages it out.
pub(super) const LINGER: Duration = Duration::from_secs(90);
/// How long a leased child may sit idle-and-parked before its row demotes to a
/// compact slate, in place in the spawn tree.
///
/// A frontend rule read off the agent's own exchange clock: nothing agent-side
/// acts at the mark.
pub(crate) const DEMOTE_IDLE: Duration = Duration::from_mins(5);
