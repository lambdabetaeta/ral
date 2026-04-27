//! Tab completion and syntax-highlighting hooks for rustyline.
//!
//! [`RalHelper`] implements rustyline's `Completer`, `Hinter`, and
//! `Highlighter`.  Completion classifies the token under the cursor as
//! variable / command / path; highlighting and ghost text come from
//! plugin buffer-change hooks recorded in [`super::plugin::PluginRuntime`].

use ral_core::Shell;
use ral_core::ansi;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use super::plugin::{PluginRuntime, lock, run_buffer_change_hooks};

// ── RalHelper ────────────────────────────────────────────────────────────

pub(super) struct RalHelper {
    pub(super) commands: Vec<String>,
    pub(super) plugin_runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) terminal: ral_core::io::TerminalState,
}

impl RalHelper {
    pub(super) fn new(shell: &Shell, plugin_runtime: Arc<Mutex<PluginRuntime>>) -> Self {
        let mut helper = RalHelper {
            commands: Vec::new(),
            plugin_runtime,
            terminal: shell.io.terminal,
        };
        helper.refresh_commands(shell);
        helper
    }

    /// Recompute `commands` from the live environment and current `$PATH`.
    /// Called once per prompt so new `let` bindings appear immediately.
    pub(super) fn refresh_commands(&mut self, shell: &Shell) {
        let mut commands: Vec<String> = shell
            .all_bindings()
            .into_iter()
            .filter_map(|(name, _)| (!name.starts_with('_')).then_some(name))
            .collect();

        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    commands.extend(
                        entries
                            .flatten()
                            .filter_map(|e| e.file_name().into_string().ok()),
                    );
                }
            }
        }

        commands.sort();
        commands.dedup();
        self.commands = commands;
        self.terminal = shell.io.terminal;
    }
}

impl RalHelper {
    /// Filter `self.commands` by case-insensitive prefix, returning rustyline `Pair`s.
    /// An empty prefix returns all commands.
    fn match_commands(&self, prefix: &str) -> Vec<Pair> {
        let lower = prefix.to_lowercase();
        self.commands
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&lower))
            .map(|c| name_pair(c))
            .collect()
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
        let (start, kind) = CompletionKind::classify(&line[..pos]);
        match kind {
            CompletionKind::Variable { prefix } | CompletionKind::Command { prefix } => {
                Ok((start, self.match_commands(prefix)))
            }
            CompletionKind::Path { token } => complete_path(token, start),
        }
    }
}

/// Classification of the token under the cursor.
enum CompletionKind<'a> {
    /// `$prefix` — complete an identifier name.
    Variable { prefix: &'a str },
    /// At command position (start of line, after `|`, `{`, `(`, `;`, `&&`, `||`).
    Command { prefix: &'a str },
    /// Anything else — complete a filesystem path.
    Path { token: &'a str },
}

impl<'a> CompletionKind<'a> {
    fn classify(before: &'a str) -> (usize, Self) {
        let token_start = before
            .rfind(|c: char| c.is_whitespace() || matches!(c, '|' | '{' | '(' | ';'))
            .map_or(0, |i| i + 1);
        let token = &before[token_start..];

        if let Some(prefix) = token.strip_prefix('$') {
            return (token_start + 1, CompletionKind::Variable { prefix });
        }

        if is_cmd_pos(before[..token_start].trim_end()) && !token.contains('/') {
            return (token_start, CompletionKind::Command { prefix: token });
        }

        (token_start, CompletionKind::Path { token })
    }
}

/// True when the cursor is at a position where a command name is expected.
fn is_cmd_pos(before_token: &str) -> bool {
    if before_token.is_empty() {
        return true;
    }
    // Single-char boundaries.
    if before_token.ends_with(['|', '{', '?', ';', '(']) {
        return true;
    }
    // Two-char operators `&&` and `||`.
    before_token.ends_with("&&") || before_token.ends_with("||")
}

fn name_pair(name: &str) -> Pair {
    Pair {
        display: name.to_string(),
        replacement: name.to_string(),
    }
}

// ── Path completion ───────────────────────────────────────────────────────

/// Returns true if `name` needs shell quoting (contains a non-bare character).
/// Mirrors `is_bare_char` in `core/src/lexer.rs`.
fn needs_quoting(name: &str) -> bool {
    name.chars().any(|c| {
        matches!(
            c,
            ' ' | '\t'
                | '\n'
                | '|'
                | '{'
                | '}'
                | '['
                | ']'
                | '$'
                | '!'
                | '~'
                | '<'
                | '>'
                | '"'
                | '\''
                | ','
                | '('
                | ')'
                | ';'
        )
    })
}

/// Shell-quote `s` in single quotes, escaping embedded single quotes as `'\''`.
fn single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Expand a tilde-prefixed directory component.
/// `"~/"` → `"{home}/"`, `"~/sub/"` → `"{home}/sub/"`.
/// Returns `None` when the home directory is unavailable.
fn expand_tilde(dir: &str) -> Option<String> {
    match dir.strip_prefix('~') {
        Some(rest) => {
            let home = crate::platform::home_dir();
            if home == "." {
                None
            } else {
                Some(format!("{home}{rest}"))
            }
        }
        None => Some(dir.to_string()),
    }
}

/// List entries of `dir` whose names start with `prefix` (case-sensitive),
/// skipping dotfiles unless the prefix itself starts with `.`.
/// Returns `(name, is_dir)` pairs.
fn dir_entries(dir: &str, prefix: &str) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            should_offer_path_candidate(&name, prefix).then(|| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (name, is_dir)
            })
        })
        .collect()
}

pub(super) fn should_offer_path_candidate(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.'))
}

pub(super) fn complete_path(
    token: &str,
    token_start: usize,
) -> rustyline::Result<(usize, Vec<Pair>)> {
    // Bare `~`: list home directory with `~/` prefix on replacements.  The
    // replacement must include `~/` because rustyline replaces from
    // `token_start`; quoting it would suppress tilde expansion, so names
    // with special chars are left bare on this path.
    if token == "~" {
        let home = crate::platform::home_dir();
        if home == "." {
            return Ok((token_start, vec![]));
        }
        return Ok((token_start, sorted_pairs(&home, "", "~/", false)));
    }

    // Split at last `/` to obtain the directory to read and the name prefix.
    let (dir, name_prefix, prefix_offset) = match token.rfind('/') {
        Some(slash) => (&token[..=slash], &token[slash + 1..], slash + 1),
        None => ("./", token, 0),
    };

    let Some(expanded) = expand_tilde(dir) else {
        return Ok((token_start + prefix_offset, vec![]));
    };

    Ok((
        token_start + prefix_offset,
        sorted_pairs(&expanded, name_prefix, "", true),
    ))
}

/// Build sorted completion pairs for entries of `dir` matching `name_prefix`.
/// Each replacement is `replacement_prefix` + name (+ `/` if dir), shell-quoted
/// when `quote` is set and the name needs it.
fn sorted_pairs(dir: &str, name_prefix: &str, replacement_prefix: &str, quote: bool) -> Vec<Pair> {
    let mut pairs: Vec<Pair> = dir_entries(dir, name_prefix)
        .into_iter()
        .map(|(name, is_dir)| {
            let display = if is_dir { format!("{name}/") } else { name.clone() };
            let body = format!("{replacement_prefix}{display}");
            let replacement = if quote && needs_quoting(&name) {
                single_quote(&body)
            } else {
                body
            };
            Pair { display, replacement }
        })
        .collect();
    pairs.sort_by(|a, b| a.display.cmp(&b.display));
    pairs
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
        lock(&self.plugin_runtime).ghost_text.clone().map(GhostHint)
    }
}

impl Highlighter for RalHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.terminal.ui_ansi_ok() {
            return Cow::Borrowed(line);
        }
        let rt = lock(&self.plugin_runtime);
        if rt.highlight_spans.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(apply_highlights(line, &rt.highlight_spans))
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

pub(super) fn apply_highlights(line: &str, spans: &[ral_core::types::HighlightSpan]) -> String {
    if spans.is_empty() {
        return line.to_string();
    }

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let len = chars.len();
    let mut styles: Vec<Option<&str>> = vec![None; len];
    for span in spans {
        for slot in &mut styles[span.start.min(len)..span.end.min(len)] {
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
                out.push_str(style_ansi(s));
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

fn style_ansi(style: &str) -> &'static str {
    match style {
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
        _ => "",
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_cmd_pos ──────────────────────────────────────────────────────

    #[test]
    fn cmd_pos_recognises_command_boundaries() {
        for s in ["", "foo |", "if true {", "x?", "foo;", "(", "foo &&", "foo ||"] {
            assert!(is_cmd_pos(s), "expected cmd pos at {s:?}");
        }
        assert!(!is_cmd_pos("foo"));
    }

    // ── Case-insensitive matching ────────────────────────────────────────

    #[test]
    fn case_insensitive_upper_prefix_matches_lower_candidate() {
        let lower = "foo".to_lowercase();
        let candidates = vec!["foobar".to_string(), "baz".to_string()];
        let matched: Vec<_> = candidates
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&lower))
            .collect();
        assert_eq!(matched, vec![&"foobar".to_string()]);
    }

    // ── needs_quoting / single_quote ────────────────────────────────────

    #[test]
    fn plain_name_no_quoting() {
        assert!(!needs_quoting("hello.txt"));
    }

    #[test]
    fn space_in_name_needs_quoting() {
        assert!(needs_quoting("my file.txt"));
    }

    #[test]
    fn single_quote_wraps_plain() {
        assert_eq!(single_quote("hello"), "'hello'");
    }

    #[test]
    fn single_quote_escapes_embedded_quote() {
        assert_eq!(single_quote("it's.txt"), "'it'\\''s.txt'");
    }

    fn safe_replacement(name: &str) -> String {
        if needs_quoting(name) {
            single_quote(name)
        } else {
            name.to_string()
        }
    }

    #[test]
    fn safe_replacement_space() {
        assert_eq!(safe_replacement("a b.txt"), "'a b.txt'");
    }

    #[test]
    fn safe_replacement_no_special() {
        assert_eq!(safe_replacement("normal.txt"), "normal.txt");
    }
}
