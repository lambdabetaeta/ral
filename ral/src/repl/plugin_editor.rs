//! Plugin context and editor state.
//!
//! Runtime state for the line-editor plugin system.  The [`PluginContext`]
//! is installed on `Shell` (via `ReplScratch.plugin_context` in core) before
//! running plugin hooks and keybinding handlers; the `_ed-*` builtins
//! defined in [`super::plugin_ed_builtins`] read and write through it
//! rather than touching shared REPL state directly.
//!
//! ## Layering
//!
//! These types live in the `ral` crate rather than `ral-core` because the
//! editor surface is purely a host concern: core compiles and runs without
//! ever inspecting the editor state.  Core stores the context in
//! `ReplScratch.plugin_context` type-erased through `Box<dyn Any>` and
//! never looks inside.
//!
//! ## Cursor unit
//!
//! Every cursor offset on this surface — [`EditorState::cursor`], the
//! [`Span`] carried by [`HighlightSpan`], and the second field of
//! [`PluginOutputs::pushed_buffer`] — is a **character** offset into the
//! buffer text, not a byte offset.  rustyline's own API uses byte offsets;
//! the REPL frontend converts at the boundary so plugin code never has to
//! think about UTF-8.  Use [`char_to_byte`] and [`byte_to_char`] for the
//! conversion.

use ral_core::Value;

/// Line editor state visible to plugins.  `cursor` is a character offset
/// into `text` (see the module-level note on cursor units).
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    pub text: std::string::String,
    pub cursor: usize,
    pub keymap: std::string::String,
}

/// A half-open character range `[start, start+len)` into buffer text.
///
/// The only constructor, [`Span::clamped`], orders its two endpoints and
/// clamps both to `[0, bound]`, so the stored range is always valid for a
/// slice of length `bound`: an inverted (`start > end`) or out-of-range
/// input folds to an in-bounds range rather than producing a panicking
/// slice.  The fields are private; the invariant cannot be bypassed.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    start: usize,
    len: usize,
}

impl Span {
    /// Build from two (possibly inverted, possibly out-of-range) character
    /// offsets and the text's character count `bound`.  Orders the pair,
    /// clamps both ends to `[0, bound]`, and stores the result as
    /// `start + len`.
    pub fn clamped(a: usize, b: usize, bound: usize) -> Span {
        let start = a.min(b).min(bound);
        let end = a.max(b).min(bound);
        Span {
            start,
            len: end - start,
        }
    }

    /// Re-clamp the range against a (possibly smaller) `bound`, so a range
    /// minted against one character count stays valid for a slice of a
    /// different one.
    pub fn clamp_to(self, bound: usize) -> Span {
        let start = self.start.min(bound);
        Span {
            start,
            len: self.len.min(bound - start),
        }
    }

    /// The half-open range `start..start+len`, in `0..=bound` for the
    /// `bound` it was clamped with.
    pub fn range(self) -> std::ops::Range<usize> {
        self.start..self.start + self.len
    }
}

/// A highlight span submitted by a plugin.  The character range is carried
/// by [`Span`] (see module note on cursor units), which keeps it ordered
/// and in bounds.
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub span: Span,
    pub style: std::string::String,
}

/// Convert a character offset into the byte offset of the same position
/// in `text`.  A `cursor` value at or past the character count returns
/// `text.len()`, so the result is always a valid slice boundary.
pub fn char_to_byte(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Convert a byte offset in `text` into the character offset of the same
/// position.  Offsets past the end are clamped to the character count; an
/// offset that lands inside a multi-byte sequence rounds down to the
/// start of that character.
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    let boundary = ral_core::text::floor_char_boundary(text, byte);
    text[..boundary].chars().count()
}

/// Execution context for `_editor` and `_plugin` builtins.
///
/// Read-only information the runtime supplies before a plugin handler runs.
#[derive(Debug, Clone, Default)]
pub struct PluginInputs {
    pub history_entries: Vec<std::string::String>,
    /// True when the handler is firing inside the readline loop (e.g. for
    /// `buffer-change`); `_ed-tui` is forbidden in that mode.
    pub in_readline: bool,
}

/// Effects produced by a plugin handler that the runtime applies after the
/// call returns.  Default-initialised before each call; populated only by the
/// handler via `_ed-*` builtins.
#[derive(Debug, Clone, Default)]
pub struct PluginOutputs {
    pub ghost_text: Option<std::string::String>,
    pub highlight_spans: Vec<HighlightSpan>,
    /// `_ed-push` saves the current buffer here for the runtime to
    /// stash on the buffer stack.  The second field is a character offset
    /// into the saved text (see module note on cursor units).
    pub pushed_buffer: Option<(std::string::String, usize)>,
    /// `_ed-accept` sets this; the runtime treats the post-call buffer
    /// as if the user pressed Enter.
    pub accept_line: bool,
}

/// Set on `Shell` before running plugin hooks/keybinding handlers.
/// The `_ed-*` builtins read and write through this rather than
/// touching shared REPL state directly, avoiding reentrancy.
///
/// The `inputs` / `outputs` split makes the data-flow direction visible at
/// every access site: callsites populate `inputs` before the call and inspect
/// `outputs` after.  `editor_state` is the live buffer (read and written by
/// the handler); `state_cell` is internal scratch.  The TUI re-entrancy
/// guard is `ReplScratch.tui_active` on the shell, not a field here.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub inputs: PluginInputs,
    pub outputs: PluginOutputs,
    /// Live editor buffer.  Pre-populated by the runtime; the handler may
    /// mutate via `_ed-set` / `_ed-push`; the runtime reads after.
    pub editor_state: EditorState,
    /// Per-plugin scratch cell exposed via `_ed-state`.
    pub state_cell: Option<Value>,
    pub state_default_used: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_round_trip_ascii() {
        let s = "hello";
        for n in 0..=s.chars().count() {
            assert_eq!(byte_to_char(s, char_to_byte(s, n)), n);
        }
    }

    #[test]
    fn char_byte_round_trip_unicode() {
        let s = "héllo🦀world";
        let nchars = s.chars().count();
        for n in 0..=nchars {
            assert_eq!(byte_to_char(s, char_to_byte(s, n)), n);
        }
    }

    #[test]
    fn char_to_byte_past_end_clamps() {
        let s = "héllo";
        assert_eq!(char_to_byte(s, 9999), s.len());
    }

    #[test]
    fn byte_to_char_past_end_clamps() {
        let s = "héllo";
        assert_eq!(byte_to_char(s, 9999), s.chars().count());
    }

    #[test]
    fn byte_to_char_inside_multibyte_rounds_down() {
        // `é` is two bytes; byte offset 2 lies between its bytes.
        let s = "hé";
        assert_eq!(byte_to_char(s, 0), 0);
        assert_eq!(byte_to_char(s, 1), 1); // start of `é`
        assert_eq!(byte_to_char(s, 2), 1); // inside `é` → rounds down
        assert_eq!(byte_to_char(s, 3), 2); // past `é`
    }

    #[test]
    fn char_to_byte_at_text_len() {
        let s = "héllo";
        let nchars = s.chars().count();
        assert_eq!(char_to_byte(s, nchars), s.len());
    }
}
