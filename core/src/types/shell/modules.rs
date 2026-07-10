//! Module loader state.
//!
//! [`Modules`] holds the active-load stack for cycle detection and the
//! current recursion depth — module loads carry no cross-session cache, so
//! every `use` / `source` re-evaluates the file fresh.  Plugin lifecycle
//! state lives in `ral::repl::plugin::PluginRuntime` — outside core, since
//! hooks and keybindings only fire inside the rustyline-driven REPL.

/// Module-loader state for `use` and `source`: the active-load stack (for
/// cycle detection) and the current recursion depth.
///
/// Both are push/pop
/// balanced within a single load, so neither flows back from a child
/// computation to its parent.
#[derive(Clone, Default, Debug)]
pub struct Modules {
    pub stack: Vec<std::string::String>,
    pub depth: usize,
}
