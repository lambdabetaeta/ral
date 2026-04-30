//! Plugin context and editor state.
//!
//! Runtime state for the line-editor plugin system.  The [`PluginContext`]
//! is installed on [`Shell`](super::Shell) before running plugin hooks and
//! keybinding handlers; `_editor` builtins read and write through it rather
//! than touching shared REPL state directly.

use super::value::Value;

/// Line editor state visible to plugins.
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    pub text: std::string::String,
    pub cursor: usize,
    pub keymap: std::string::String,
}

/// A highlight span submitted by a plugin.
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub style: std::string::String,
}

/// Execution context for `_editor` and `_plugin` builtins.
///
/// Read-only information the runtime supplies before a plugin handler runs.
#[derive(Debug, Clone, Default)]
pub struct PluginInputs {
    pub history_entries: Vec<std::string::String>,
    /// True when the handler is firing inside the readline loop (e.g. for
    /// `buffer-change`); `_editor 'tui'` is forbidden in that mode.
    pub in_readline: bool,
}

/// Effects produced by a plugin handler that the runtime applies after the
/// call returns.  Default-initialised before each call; populated only by the
/// handler via `_editor` builtins.
#[derive(Debug, Clone, Default)]
pub struct PluginOutputs {
    pub ghost_text: Option<std::string::String>,
    pub highlight_spans: Vec<HighlightSpan>,
    /// `_editor 'push'` saves the current buffer here for the runtime to
    /// stash on the buffer stack.
    pub pushed_buffer: Option<(std::string::String, usize)>,
    /// `_editor 'accept'` sets this; the runtime treats the post-call buffer
    /// as if the user pressed Enter.
    pub accept_line: bool,
}

/// Set on `Shell` before running plugin hooks/keybinding handlers.
/// The `_editor` builtins read and write through this rather than
/// touching shared REPL state directly, avoiding reentrancy.
///
/// The `inputs` / `outputs` split makes the data-flow direction visible at
/// every access site: callsites populate `inputs` before the call and inspect
/// `outputs` after.  `editor_state` is the live buffer (read and written by
/// the handler); `state_cell` and `in_tui` are internal scratch.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub inputs: PluginInputs,
    pub outputs: PluginOutputs,
    /// Live editor buffer.  Pre-populated by the runtime; the handler may
    /// mutate via `_editor 'set'`/`'push'`; the runtime reads after.
    pub editor_state: EditorState,
    /// Reentrancy guard for `_editor 'tui'`; not user-visible.
    pub in_tui: bool,
    /// Per-plugin scratch cell exposed via `_editor 'state'`.
    pub state_cell: Option<Value>,
    pub state_default_used: bool,
}
