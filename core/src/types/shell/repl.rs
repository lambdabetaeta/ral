//! REPL-only scratch state.
//!
//! State that has meaning *only* for the interactive shell and its
//! editor — not part of the language semantics, not part of any wire
//! format, and never installed on a sandbox subprocess.
//!
//! Two fields:
//!
//! - `plugin_context`: execution context for `_ed-*` builtins.  The
//!   REPL sets it before running plugin hooks and keybinding handlers
//!   so those builtins can talk to the editor (e.g. read the input
//!   buffer, set the prompt).  Type-erased (`Box<dyn Any>`) because the
//!   concrete `PluginContext` type lives in the `ral` crate — core
//!   stores the slot but never inspects its contents.
//! - `pending_chpwd`: queued (old, new) directory pair set by `cd`
//!   when called inside the evaluator.  The REPL drains it after
//!   `evaluate` returns, fires the `chpwd` lifecycle hook, and clears
//!   the field.  The process cwd is changed synchronously by `cd`;
//!   only the hook fires asynchronously.
//!
//! (The `_ed-tui` foreground signal no longer lives here: it is a
//! within-turn elevation of the turn's
//! [`TerminalAccess`](crate::types::TurnState) to `ExplicitLoan`, which the
//! turn frame flows into same-thread bodies on its own.)
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
//! - **Thread spawn (TS)**: no field flows.  Spawned threads have no
//!   editor; `ReplScratch::default()` is fine.
//! - **Sandbox IPC**: not transmitted; sandbox children get a fresh
//!   `ReplScratch`.

/// Editor-only state.  Move-rich on STT (preserve via `.take()` patterns
/// in `inherit_from` / `return_to`), absent on TS, not transmitted on
/// IPC.
#[derive(Default)]
pub struct ReplScratch {
    /// Type-erased plugin execution context.  The concrete type lives in
    /// the `ral` crate (`PluginContext`); core only knows it's a heap
    /// blob carried across the editor-builtin boundary.  `Send + Sync`
    /// because the slot may flow through `Shell::spawn_thread` in
    /// nested capture paths.
    pub plugin_context: Option<Box<dyn std::any::Any + Send + Sync>>,
    pub pending_chpwd: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

impl std::fmt::Debug for ReplScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplScratch")
            .field(
                "plugin_context",
                &self.plugin_context.as_ref().map(|_| "<opaque>"),
            )
            .field("pending_chpwd", &self.pending_chpwd)
            .finish()
    }
}

impl ReplScratch {
    /// STT-in: move `plugin_context` out of parent for the child's
    /// duration.  The editor scratch must not be visible on both sides
    /// simultaneously.  `pending_chpwd` starts fresh.  (The `_ed-tui`
    /// foreground signal now rides the turn's `TerminalAccess`, flowed by
    /// `TurnState::inherit_from`, so it is no longer carried here.)
    pub fn inherit_from(&mut self, parent: &mut ReplScratch) {
        self.plugin_context = parent.plugin_context.take();
    }

    /// STT-out: move `plugin_context` back.  If the child queued a
    /// `pending_chpwd` — `cd` inside a thunk is a real process-state
    /// change — flow it up to the parent so the REPL can fire `chpwd`
    /// after the top-level `evaluate` returns.  Don't clobber an outer
    /// pending pair: only overwrite when the child has one queued.
    pub fn return_to(&mut self, parent: &mut ReplScratch) {
        parent.plugin_context = self.plugin_context.take();
        if let Some(pair) = self.pending_chpwd.take() {
            parent.pending_chpwd = Some(pair);
        }
    }
}
