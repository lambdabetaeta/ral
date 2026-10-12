//! Line-editing frontends.
//!
//! The [`Frontend`] trait exposes one read method that returns typed
//! [`Read`] events; the REPL session never sees keybindings, plugin
//! messages, escape sequences, or the internal buffer stack — each
//! backend owns those concerns itself.
//!
//! Two implementations live in submodules:
//! - [`minimal::MinimalFrontend`] — canonical-stdin fallback for dumb
//!   terminals and `RAL_INTERACTIVE_MODE=minimal`.  No raw mode, no
//!   plugin features.
//! - [`rustyline::RustylineFrontend`] — full editor with completion,
//!   plugin keybindings, ghost text, highlights, and rustyline history.
//!
//! Every frontend reads the engine only through the transport — probes, and
//! hook dispatches — never a `Shell`.

#[cfg(unix)]
mod input;
mod minimal;
mod rustyline;

#[cfg(unix)]
pub(super) use input::Input;

pub(super) use minimal::MinimalFrontend;
pub(super) use rustyline::RustylineFrontend;

use ral_core::carrier::Transport;

use super::host::Printer;
use super::prompt::PromptText;

// ── Surface selection ───────────────────────────────────────────────────────

/// Which interactive surface the REPL presents, chosen by the `--surface`
/// flag or the rc `surface:` key (flag wins).
///
/// Distinct from [`InteractiveMode`](ral_core::terminal::InteractiveMode): that
/// records what the terminal *can* do (ANSI, round-trips) and is the escape
/// hatch for hostile setups; this records which frontend the user *wants*.
/// The capability gate overrides this — a terminal resolved to
/// [`Minimal`](ral_core::terminal::InteractiveMode::Minimal) gets the canonical
/// editor whatever surface was asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Surface {
    /// Use a basic line editor with no raw terminal mode or plugin features.
    Minimal,
    /// Use the full line editor with completion, history, plugin keybindings,
    /// suggested text and highlighting. This is the default.
    #[default]
    Readline,
}

// ── Event types ───────────────────────────────────────────────────────────

/// Buffer state to pre-fill the editor with on the next [`Frontend::read`].
///
/// `cursor` is a **character** offset into `text` — matching the units
/// [`crate::repl::plugin::editor::EditorState`] uses on the plugin surface.  The
/// frontend converts to a byte offset at the rustyline boundary;
/// out-of-range values are clamped.
#[derive(Clone, Debug, Default)]
pub(super) struct EditBuffer {
    pub(super) text: String,
    pub(super) cursor: usize,
}

/// What [`Frontend::read`] returns to the session loop.
///
/// `Line` is a complete logical input ready to evaluate; the frontend
/// has already handled any plugin keybinding that accepted.  `Edit` is
/// a buffer the session should re-feed to the next `read` call (a plugin
/// keybinding handler asked to re-edit, or `_ed-push` queued a buffer).
/// `Interrupt` is Ctrl-C at an empty prompt; `End` ends the session with
/// that exit status (0 on Ctrl-D at an empty prompt, 1 on a read error).
pub(super) enum Read {
    Line(String),
    Edit(EditBuffer),
    Interrupt,
    End(u8),
}

// ── Trait ─────────────────────────────────────────────────────────────────

pub(super) trait Frontend {
    /// Read one logical input from the user.
    ///
    /// `pending` is the buffer state the session received as a previous
    /// [`Read::Edit`] (and now wants to re-feed) — when `None`, the
    /// frontend may still draw initial state from its own internal queue
    /// (e.g. `_ed-push`).  The frontend is responsible for plugin sync,
    /// keybinding dispatch and line-erase escapes before returning; the session
    /// flushes deferred plugin diagnostics afterwards.
    fn read(
        &mut self,
        engine: &dyn Transport,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
        #[cfg(unix)] input: &mut Input,
    ) -> Read;

    fn add_history(&mut self, entry: &str);
    fn save_history(&mut self) -> Result<(), String>;

    /// Where a line printed while the user edits should go, if this frontend
    /// can put it above the prompt.
    fn printer(&mut self) -> Option<Printer> {
        None
    }
}
