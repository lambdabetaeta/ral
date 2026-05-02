//! REPL-only scratch state.
//!
//! State that has meaning *only* for the interactive shell and its
//! editor — not part of the language semantics, not part of any wire
//! format, and never installed on a sandbox subprocess.
//!
//! Two fields:
//!
//! - `plugin_context`: execution context for `_editor` builtins.  The
//!   REPL sets it before running plugin hooks and keybinding handlers
//!   so those builtins can talk to the editor (e.g. read the input
//!   buffer, set the prompt).
//! - `pending_chpwd`: queued (old, new) directory pair set by `cd`
//!   when called inside the evaluator.  The REPL drains it after
//!   `evaluate` returns, fires the `chpwd` lifecycle hook, and clears
//!   the field.  The process cwd is changed synchronously by `cd`;
//!   only the hook fires asynchronously.
//!
//! Flow rules:
//!
//! - **Same-thread thunk (STT)**: `plugin_context` is *moved* (`.take()`)
//!   from parent into child on `inherit_from`, and moved back on
//!   `return_to`.  This is intentional: while the child is running the
//!   parent must not see the editor scratch.  `pending_chpwd` is
//!   fresh-on-child and *flows back* on `return_to` if the child queued
//!   one — `cd` inside a thunk is a real process-state change, and the
//!   REPL must fire `chpwd` for it just like it does for top-level cd.
//! - **Thread spawn (TS)**: neither field flows.  Spawned threads have
//!   no editor; `ReplScratch::default()` is fine.
//! - **Sandbox IPC**: not transmitted; sandbox children get a fresh
//!   `ReplScratch`.

use crate::types::PluginContext;

/// Editor-only state.  Move-rich on STT (preserve via `.take()` patterns
/// in `inherit_from` / `return_to`), absent on TS, not transmitted on
/// IPC.
#[derive(Debug, Default)]
pub struct ReplScratch {
    pub plugin_context: Option<PluginContext>,
    pub pending_chpwd: Option<(std::path::PathBuf, std::path::PathBuf)>,
}
