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

mod minimal;
mod rustyline;

pub(super) use minimal::MinimalFrontend;
pub(super) use rustyline::RustylineFrontend;

use ral_core::Shell;

use super::prompt::PromptText;

// ── Event types ───────────────────────────────────────────────────────────

/// Buffer state to pre-fill the editor with on the next [`Frontend::read`].
///
/// `cursor` is a **character** offset into `text` — matching the units
/// [`ral_core::types::EditorState`] uses on the plugin surface.  The
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
/// `Interrupt` is Ctrl-C at an empty prompt; `Eof` is Ctrl-D on empty.
pub(super) enum Read {
    Line(String),
    Edit(EditBuffer),
    Interrupt,
    Eof,
}

// ── Trait ─────────────────────────────────────────────────────────────────

/// One backend-supplied continuation read, classified for
/// [`join_continuation`].
pub(super) enum Continuation {
    /// Another line to fold into the buffer.
    Line(String),
    /// Ctrl-C / Ctrl-D / EOF: abandon the partial buffer, yielding an
    /// empty input for the session to skip.
    Discard,
}

/// Fold continuation lines into `first` until the buffer is a complete
/// input, joining with newlines.  The backend owns the actual read (and
/// its own prompt / abort detection) via `next`; this drives the shared
/// `needs_continuation` loop both frontends previously duplicated — the
/// site where the Ctrl-D divergence crept in.  A [`Continuation::Discard`]
/// returns the empty string.
pub(super) fn join_continuation(first: String, mut next: impl FnMut() -> Continuation) -> String {
    let mut buf = first;
    while ral_core::syntax::parser::needs_continuation(&buf) {
        match next() {
            Continuation::Line(line) => {
                buf.push('\n');
                buf.push_str(&line);
            }
            Continuation::Discard => return String::new(),
        }
    }
    buf
}

pub(super) trait Frontend {
    /// Read one logical input from the user.
    ///
    /// `pending` is the buffer state the session received as a previous
    /// [`Read::Edit`] (and now wants to re-feed) — when `None`, the
    /// frontend may still draw initial state from its own internal queue
    /// (e.g. `_ed-push`).  The frontend is responsible for plugin sync,
    /// keybinding dispatch, continuation reads, line-erase escapes, and
    /// flushing deferred plugin diagnostics before returning.
    fn read(&mut self, shell: &mut Shell, prompt: &PromptText, pending: Option<EditBuffer>)
    -> Read;

    fn add_history(&mut self, entry: &str);
    fn save_history(&mut self);
}

#[cfg(test)]
mod tests {
    use super::{Continuation, join_continuation};

    /// A complete first line is returned without reading any continuation.
    #[test]
    fn complete_input_reads_no_continuation() {
        let mut calls = 0;
        let out = join_continuation("echo hi".into(), || {
            calls += 1;
            Continuation::Discard
        });
        assert_eq!(out, "echo hi");
        assert_eq!(calls, 0, "a complete line must not prompt for more");
    }

    /// Continuation lines are folded in with newlines until the buffer
    /// parses as complete.
    #[test]
    fn folds_lines_until_complete() {
        let mut rest = vec!["world"].into_iter();
        // A trailing pipe is an incomplete input; the next line completes it.
        let out = join_continuation("echo hi |".into(), || {
            Continuation::Line(rest.next().unwrap().to_string())
        });
        assert_eq!(out, "echo hi |\nworld");
    }

    /// Discard (Ctrl-C / Ctrl-D / read error) abandons the partial buffer.
    #[test]
    fn discard_yields_empty() {
        let out = join_continuation("echo hi |".into(), || Continuation::Discard);
        assert_eq!(out, "");
    }
}
