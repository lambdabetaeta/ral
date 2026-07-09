//! Rustyline adapter over the shared completion engine, plus the
//! syntax-highlighting and ghost-text hooks.
//!
//! [`RalHelper`] implements rustyline's `Completer`, `Hinter`, and
//! `Highlighter`.  Completion itself is the frontend-neutral
//! [`super::completion`] engine; this type only holds the per-prompt
//! [`Sources`](super::completion::Sources) snapshot and adapts the engine's
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
use super::plugin::{PluginRuntime, lock, run_buffer_change_hooks};
use super::plugin_editor::HighlightSpan;

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
        RalHelper {
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

// ── Syntax highlighting ──────────────────────────────────────────────────

pub(super) fn apply_highlights(line: &str, spans: &[HighlightSpan]) -> String {
    if spans.is_empty() {
        return line.to_string();
    }

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let len = chars.len();
    let mut styles: Vec<Option<&str>> = vec![None; len];
    for span in spans {
        for slot in &mut styles[span.span.clamp_to(len).range()] {
            *slot = Some(span.style.as_str());
        }
    }

    let mut out = String::with_capacity(line.len() * 2);
    let mut cur: Option<&str> = None;
    for (i, &(_, ch)) in chars.iter().enumerate() {
        let new = styles[i];
        if new != cur {
            if cur.is_some() {
                out.push_str(ansi::RESET);
            }
            if let Some(s) = new {
                out.push_str(style_ansi(s).unwrap_or(""));
            }
            cur = new;
        }
        out.push(ch);
    }
    if cur.is_some() {
        out.push_str(ansi::RESET);
    }
    out
}

/// Map a highlight style name to its ANSI escape, or `None` if the name is not
/// a known style.  The single source of truth for the legal style vocabulary —
/// `_ed-highlight` derives its validation from this.
#[allow(clippy::match_same_arms)]
pub(super) fn style_ansi(style: &str) -> Option<&'static str> {
    Some(match style {
        "command" => ansi::BOLD_GREEN,
        "builtin" => ansi::BOLD_CYAN,
        "prelude" => ansi::BOLD_BLUE,
        "argument" => "",
        "option" => ansi::CYAN,
        "path-exists" => ansi::UNDERLINE,
        "path-missing" => ansi::UNDERLINE_RED,
        "string" => ansi::YELLOW,
        "number" => ansi::MAGENTA,
        "comment" => ansi::DIM,
        "error" => ansi::BOLD_RED,
        "match" => ansi::BOLD,
        "bracket-1" => ansi::CYAN,
        "bracket-2" => ansi::MAGENTA,
        "bracket-3" => ansi::YELLOW,
        _ => return None,
    })
}

/// Map a highlight style name to a ratatui [`Style`](ratatui::style::Style),
/// or `None` for an unknown name — the structural surface's analogue of
/// [`style_ansi`], which paints into a terminal cell rather than emitting an
/// ANSI escape.  Kept adjacent and arm-for-arm with `style_ansi` so the two
/// stay in lockstep: a style added to the vocabulary must be given both an
/// escape and a cell style.
#[cfg(feature = "structural")]
#[allow(clippy::match_same_arms)]
pub(super) fn style_ratatui(style: &str) -> Option<ratatui::style::Style> {
    use ratatui::style::{Color, Modifier, Style};
    let s = Style::default();
    Some(match style {
        "command" => s.fg(Color::Green).add_modifier(Modifier::BOLD),
        "builtin" => s.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "prelude" => s.fg(Color::Blue).add_modifier(Modifier::BOLD),
        "argument" => s,
        "option" => s.fg(Color::Cyan),
        "path-exists" => s.add_modifier(Modifier::UNDERLINED),
        "path-missing" => s.fg(Color::Red).add_modifier(Modifier::UNDERLINED),
        "string" => s.fg(Color::Yellow),
        "number" => s.fg(Color::Magenta),
        "comment" => s.add_modifier(Modifier::DIM),
        "error" => s.fg(Color::Red).add_modifier(Modifier::BOLD),
        "match" => s.add_modifier(Modifier::BOLD),
        "bracket-1" => s.fg(Color::Cyan),
        "bracket-2" => s.fg(Color::Magenta),
        "bracket-3" => s.fg(Color::Yellow),
        _ => return None,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::plugin_editor::Span;

    // ── apply_highlights span safety ─────────────────────────────────────
    //
    // `Span::clamped` orders its endpoints, so an inverted `(start, end)`
    // submitted by a plugin folds to the same range as the ordered pair and
    // can never produce a backwards slice in `apply_highlights`.

    #[test]
    fn apply_highlights_inverted_span_folds_to_ordered() {
        let line = "hello world";
        let bound = line.chars().count();
        let inverted = HighlightSpan {
            span: Span::clamped(5, 2, bound),
            style: "command".into(),
        };
        let ordered = HighlightSpan {
            span: Span::clamped(2, 5, bound),
            style: "command".into(),
        };
        assert_eq!(
            apply_highlights(line, &[inverted]),
            apply_highlights(line, &[ordered]),
        );
    }

    #[test]
    fn apply_highlights_out_of_range_span_clamps() {
        let line = "hi";
        let span = HighlightSpan {
            span: Span::clamped(0, 999, line.chars().count()),
            style: "command".into(),
        };
        // Must not panic: the range is clamped to the slice length.
        let _ = apply_highlights(line, &[span]);
    }

    #[test]
    fn apply_highlights_span_reclamps_to_shorter_line() {
        // A span minted against a longer buffer must not panic when the line
        // rendered at the slice site has since shrunk.
        let span = HighlightSpan {
            span: Span::clamped(0, 10, 10),
            style: "command".into(),
        };
        let _ = apply_highlights("hi", &[span]);
    }
}
