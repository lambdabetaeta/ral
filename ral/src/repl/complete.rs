//! Rustyline adapter over the shared completion engine, plus the
//! syntax-highlighting and ghost-text hooks.
//!
//! [`RalHelper`] implements rustyline's `Completer`, `Hinter`, and
//! `Highlighter`.  Completion itself is the frontend-neutral
//! [`super::completion`] engine; this type only holds the per-prompt
//! [`Sources`] snapshot and adapts the engine's
//! [`Candidate`](super::completion::Candidate)s to rustyline `Pair`s.
//! Highlighting and ghost text come from plugin buffer-change hooks recorded
//! in [`super::plugin::PluginRuntime`].

use ral_core::Shell;
use ral_core::ansi;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use super::completion::{self, Sources};
use super::highlight_style::apply_highlights;
use super::plugin::{PluginRuntime, lock, run_buffer_change_hooks};

// ── RalHelper ────────────────────────────────────────────────────────────

pub(super) struct RalHelper {
    /// The per-prompt snapshot of command/variable names and cwd the
    /// completion engine ranks against, rebuilt by [`RalHelper::refresh`].
    sources: Sources,
    pub(super) plugin_runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) terminal: ral_core::io::TerminalState,
}

impl RalHelper {
    pub(super) fn new(shell: &Shell, plugin_runtime: Arc<Mutex<PluginRuntime>>) -> Self {
        Self {
            sources: Sources::from_shell(shell),
            plugin_runtime,
            terminal: shell.terminal(),
        }
    }

    /// Rebuild completion state from the live shell.  Called once per prompt
    /// so new `let` bindings, `within [shell: PATH=…]` overrides, and
    /// `cd`-tracked cwd changes appear immediately.
    pub(super) fn refresh(&mut self, shell: &Shell) {
        self.sources = Sources::from_shell(shell);
        self.terminal = shell.terminal();
    }
}

impl Completer for RalHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, candidates) = completion::complete(line, pos, &self.sources);
        let pairs = candidates
            .into_iter()
            .map(|c| Pair {
                display: c.display,
                replacement: c.replacement,
            })
            .collect();
        Ok((start, pairs))
    }
}

// ── Hinter / Highlighter / Validator / Helper ────────────────────────────

/// Ghost-text hint returned by `Hinter`.
///
/// Wraps the suggestion suffix so that `completion()` returns the text,
/// enabling rustyline to insert it on right-arrow at end-of-line.
pub(super) struct GhostHint(String);

impl rustyline::hint::Hint for GhostHint {
    fn display(&self) -> &str {
        &self.0
    }
    fn completion(&self) -> Option<&str> {
        Some(&self.0)
    }
}

impl Hinter for RalHelper {
    type Hint = GhostHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<GhostHint> {
        run_buffer_change_hooks(&self.plugin_runtime, line, pos);
        lock(&self.plugin_runtime)
            .hooks
            .ghost
            .clone()
            .map(GhostHint)
    }
}

impl Highlighter for RalHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.terminal.ui_ansi_ok() {
            return Cow::Borrowed(line);
        }
        let rt = lock(&self.plugin_runtime);
        if rt.hooks.highlights.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(apply_highlights(line, &rt.hooks.highlights))
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if !self.terminal.ui_ansi_ok() {
            return Cow::Borrowed(hint);
        }
        Cow::Owned(format!("{}{hint}{}", ansi::DIM, ansi::RESET))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        // Re-highlight every keystroke so plugin spans stay in sync.
        // Skip entirely on terminals that cannot display ANSI.
        self.terminal.ui_ansi_ok()
    }
}

impl Validator for RalHelper {}
impl Helper for RalHelper {}
