//! REPL-only editor scratch on `Shell.local`: no part of the language
//! semantics, in no wire format.  Only `Shell::child_of` / `Shell::return_to`
//! carry it across a fork; every other fork starts from `default()`.

/// The channel between core's builtins and the host's line editor.
#[derive(Default)]
pub struct ReplScratch {
    /// Type-erased `PluginContext` — it lives in the `ral` crate, so core
    /// holds the slot and never looks inside.  `Send` because the whole
    /// `Shell` moves onto the engine's worker thread.
    pub plugin_context: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// `(old, new)` queued by `cd`, which moves only the shell's logical cwd;
    /// the REPL drains it after a dispatch to fire the `chpwd` hook.
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
    /// Lend the parent's `plugin_context` to the child for the stage's
    /// duration; pair with `return_to` or it dies with the child.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.plugin_context = parent.plugin_context.take();
    }

    /// Repay the loan and raise the child's queued `chpwd` — only if it queued
    /// one, lest an outer pending pair be clobbered by a silent `None`.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.plugin_context = self.plugin_context.take();
        if let Some(pair) = self.pending_chpwd.take() {
            parent.pending_chpwd = Some(pair);
        }
    }
}
