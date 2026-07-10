//! REPL-only scratch state.
//!
//! [`ReplScratch`] holds editor state with meaning *only* for the
//! interactive shell and its editor: no part of the language semantics,
//! no part of any wire format, and never installed on a sandbox
//! subprocess.  Its two fields flow between parent and child only along
//! the cross-process pipeline-stage manifest (`Shell::child_of` /
//! `Shell::return_to`, which call [`ReplScratch::inherit_from`] /
//! [`ReplScratch::return_to`]); a spawned thread and a sandbox child each
//! start from a fresh `ReplScratch::default()`.

/// Editor-only scratch, carried across the cross-process pipeline-stage
/// boundary via [`Self::inherit_from`] / [`Self::return_to`]; absent on a
/// spawned thread, not transmitted on sandbox IPC.
#[derive(Default)]
pub struct ReplScratch {
    /// Type-erased plugin execution context for `_ed-*` builtins.  The
    /// REPL sets it before running plugin hooks and keybinding handlers so
    /// those builtins can talk to the editor (read the input buffer, set
    /// the prompt).  The concrete `PluginContext` type lives in the `ral`
    /// crate; core stores the slot but never inspects it.  `Send + Sync`
    /// because it may flow through `Shell::spawn_thread` in nested capture
    /// paths.
    pub plugin_context: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// Queued `(old, new)` directory pair set by `cd` inside the
    /// evaluator.  The REPL drains it after `evaluate` returns, fires the
    /// `chpwd` lifecycle hook, and clears the field.  The process cwd is
    /// changed synchronously by `cd`; only the hook fires afterward.
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
    /// Move `plugin_context` out of parent for the child stage's duration;
    /// `pending_chpwd` starts fresh on the child.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.plugin_context = parent.plugin_context.take();
    }

    /// Move `plugin_context` back to the parent.  If the child queued a
    /// `pending_chpwd` — a `cd` in the stage is a real process-state
    /// change — flow it up so the REPL can fire `chpwd` after the top-level
    /// `evaluate` returns.  Don't clobber an outer pending pair: only
    /// overwrite when the child has one queued.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.plugin_context = self.plugin_context.take();
        if let Some(pair) = self.pending_chpwd.take() {
            parent.pending_chpwd = Some(pair);
        }
    }
}
