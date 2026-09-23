//! The editor context the host installs around a plugin hook's dispatch, and
//! the `` `repl-editor `` requests the engine's `_ed-*` doors put to it.
//!
//! ## Cursor unit
//!
//! Every cursor offset on this surface — [`EditorState::cursor`], the
//! [`Span`] carried by [`HighlightSpan`], and the second field of
//! [`PluginOutputs::pushed_buffer`] — is a **character** offset into the
//! buffer text, not a byte offset.  rustyline's own API uses byte offsets;
//! the REPL frontend converts at the boundary so plugin code never has to
//! think about UTF-8.  Use [`ral_core::text::char_to_byte`] and
//! [`ral_core::text::byte_to_char`] for the conversion.

use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum as _;

use super::super::enquiry::{Data, EditorOp, EditorSnapshot};

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
    /// Build a span from two character offsets and the text's character count
    /// `bound`.  See the struct invariant for the ordering and clamping applied.
    pub fn clamped(a: usize, b: usize, bound: usize) -> Self {
        let start = a.min(b).min(bound);
        let end = a.max(b).min(bound);
        Self {
            start,
            len: end - start,
        }
    }

    /// Re-clamp the range against a (possibly smaller) `bound`, so a range
    /// minted against one character count stays valid for a slice of a
    /// different one.
    pub fn clamp_to(self, bound: usize) -> Self {
        let start = self.start.min(bound);
        Self {
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

/// The editor context of one plugin hook's dispatch: the live buffer, what
/// the handler may read, what it produced, and the plugin's state cell.
#[derive(Debug, Clone, Default)]
pub struct PluginContext {
    pub editor_state: EditorState,
    pub history: Vec<std::string::String>,
    /// True inside the readline loop (`buffer-change`), where `_ed-tui` is
    /// forbidden.
    pub in_readline: bool,
    pub outputs: PluginOutputs,
    pub state_cell: Option<FOValue>,
}

/// Split `text` at a character-offset `cursor` into the substrings left and
/// right of it.
pub(crate) fn split_at_cursor(text: &str, cursor: usize) -> (String, String) {
    let left: String = text.chars().take(cursor).collect();
    let right: String = text.chars().skip(cursor).collect();
    (left, right)
}

impl PluginContext {
    /// Answer one request.
    pub(crate) fn apply(&mut self, op: EditorOp) -> FOValue {
        let st = &mut self.editor_state;
        match op {
            EditorOp::Get => {
                return EditorSnapshot {
                    text: st.text.clone(),
                    cursor: st.cursor,
                    keymap: st.keymap.clone(),
                    in_readline: self.in_readline,
                }
                .encode();
            }
            EditorOp::History => return self.history.clone().encode(),
            EditorOp::StateGet => return self.state_cell.clone().map(Data).encode(),
            // The cursor clamps against the *new* text.
            EditorOp::Set { text, cursor } => {
                if let Some(text) = text {
                    st.text = text;
                }
                if let Some(n) = cursor {
                    st.cursor = n.min(st.text.chars().count());
                }
            }
            EditorOp::SetLbuffer(left) => {
                let (_, right) = split_at_cursor(&st.text, st.cursor);
                st.cursor = left.chars().count();
                st.text = format!("{left}{right}");
            }
            EditorOp::Insert(s) => {
                let (left, right) = split_at_cursor(&st.text, st.cursor);
                st.cursor += s.chars().count();
                st.text = format!("{left}{s}{right}");
            }
            EditorOp::Push => {
                let text = std::mem::take(&mut st.text);
                self.outputs.pushed_buffer = Some((text, std::mem::take(&mut st.cursor)));
            }
            EditorOp::Accept => self.outputs.accept_line = true,
            EditorOp::Ghost(text) => self.outputs.ghost_text = (!text.is_empty()).then_some(text),
            EditorOp::Highlight(spans) => {
                let bound = st.text.chars().count();
                self.outputs.highlight_spans = spans
                    .into_iter()
                    .map(|h| HighlightSpan {
                        span: Span::clamped(h.start, h.end, bound),
                        style: h.style,
                    })
                    .collect();
            }
            EditorOp::StateSet(Data(v)) => self.state_cell = Some(v),
        }
        FOValue::Unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_text(text: &str) -> PluginContext {
        let mut pc = PluginContext::default();
        pc.editor_state.text = text.to_string();
        pc
    }

    /// Setting `text` and `cursor` together clamps the cursor against the
    /// **new** text, not the old (shorter) buffer.
    #[test]
    fn set_clamps_cursor_against_new_text() {
        let mut pc = with_text("");
        pc.apply(EditorOp::Set {
            text: Some("hello world".into()),
            cursor: Some(11),
        });
        assert_eq!(pc.editor_state.text, "hello world");
        assert_eq!(pc.editor_state.cursor, 11);
    }

    /// An over-long cursor clamps to the new text's character count.
    #[test]
    fn set_clamps_over_long_cursor_to_new_len() {
        let mut pc = with_text("");
        pc.apply(EditorOp::Set {
            text: Some("abc".into()),
            cursor: Some(99),
        });
        assert_eq!(pc.editor_state.cursor, 3);
    }
}
