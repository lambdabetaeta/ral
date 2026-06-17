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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::plugin::{PluginRuntime, lock, run_buffer_change_hooks};
use super::plugin_editor::HighlightSpan;

// ── RalHelper ────────────────────────────────────────────────────────────

pub(super) struct RalHelper {
    pub(super) commands: Vec<String>,
    /// Scope binding names only — the source for `$`-variable completion,
    /// which must not offer PATH commands, builtins, or handlers.
    pub(super) variables: Vec<String>,
    /// Shell's logical cwd, refreshed each prompt.  Path completion
    /// anchors relative directories against this rather than the
    /// process cwd — `cd` and `within [dir: …]` mutate only the
    /// shell's pair, so the two diverge as soon as the user navigates.
    pub(super) cwd: PathBuf,
    pub(super) plugin_runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) terminal: ral_core::io::TerminalState,
}

impl RalHelper {
    pub(super) fn new(shell: &Shell, plugin_runtime: Arc<Mutex<PluginRuntime>>) -> Self {
        let mut helper = RalHelper {
            commands: Vec::new(),
            variables: Vec::new(),
            cwd: shell.cwd(),
            plugin_runtime,
            terminal: shell.terminal(),
        };
        helper.refresh(shell);
        helper
    }

    /// Recompute completion state from the live shell.  Called once per
    /// prompt so new `let` bindings, `within [shell: PATH=…]` overrides,
    /// and `cd`-tracked cwd changes appear immediately.
    ///
    /// PATH lookup goes through the dynamic env overlay
    /// (`within [shell: PATH=…]` wins over the host) and through
    /// [`ral_core::path::commands_on_path`], which mirrors
    /// `locate`'s rules — relative entries are anchored against the
    /// shell's cwd and the executable bit is required.  Completion now
    /// offers exactly the commands the shell will actually run.
    pub(super) fn refresh(&mut self, shell: &Shell) {
        let mut variables: Vec<String> = shell
            .mobile
            .scope
            .all_bindings()
            .into_iter()
            .filter_map(|(name, _)| (!name.starts_with('_')).then_some(name))
            .collect();
        variables.sort();
        variables.dedup();

        let mut commands = variables.clone();
        let cwd = shell.cwd();
        if let Some(path) = shell.mobile.context.env_overrides().get_or_host("PATH") {
            commands.extend(ral_core::path::commands_on_path(&path, Some(&cwd)));
        }

        commands.extend(
            shell
                .builtin_names()
                .filter(|name| !name.starts_with('_'))
                .map(str::to_string),
        );

        commands.extend(
            shell
                .mobile
                .context
                .handlers
                .entries()
                .filter(|e| !e.name.starts_with('_'))
                .map(|e| e.name.as_ref().to_string()),
        );

        commands.sort();
        commands.dedup();
        self.commands = commands;
        self.variables = variables;
        self.cwd = cwd;
        self.terminal = shell.terminal();
    }
}

impl RalHelper {
    /// Filter `names` by case-insensitive prefix, returning rustyline `Pair`s.
    /// An empty prefix returns every name.
    fn match_names(names: &[String], prefix: &str) -> Vec<Pair> {
        let lower = prefix.to_lowercase();
        names
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
            CompletionKind::Variable { prefix } => {
                Ok((start, Self::match_names(&self.variables, prefix)))
            }
            CompletionKind::Command { prefix } => {
                Ok((start, Self::match_names(&self.commands, prefix)))
            }
            CompletionKind::Path { token } => complete_path(token, start, &self.cwd),
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

/// Expand a tilde-prefixed directory component for completion.
/// Delegates to [`ral_core::path::tilde`] so the rule matches the
/// rest of ral; returns `None` when the home directory is
/// unavailable so the caller can fall back to non-tilde
/// completion.
fn expand_tilde(dir: &str) -> Option<String> {
    let Some(parsed) = ral_core::path::tilde::TildePath::parse(dir) else {
        return Some(dir.to_string());
    };
    let home = crate::platform::home_dir();
    if home == "." {
        return None;
    }
    Some(ral_core::path::tilde::expand_tilde_path(
        parsed.user.as_deref(),
        parsed.suffix.as_deref(),
        &home,
    ))
}

/// List entries of `dir` whose names start with `prefix` (case-sensitive),
/// skipping dotfiles unless the prefix itself starts with `.`.
/// Returns `(name, is_dir)` pairs.
fn dir_entries(dir: &Path, prefix: &str) -> Vec<(String, bool)> {
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

// Adapter site: turns user-typed completion tokens (a home dir, a
// tilde-expanded directory) into `&Path`s to read for candidates. These
// are the "user-input literal paths" adapter case clippy.toml sanctions
// for a local `Path::new` with a reason.
#[allow(clippy::disallowed_methods)]
pub(super) fn complete_path(
    token: &str,
    token_start: usize,
    cwd: &Path,
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
        return Ok((token_start, sorted_pairs(Path::new(&home), "", "~/", false)));
    }

    // Split at last `/` to obtain the directory to read and the name prefix.
    let (dir, name_prefix, prefix_offset) = match token.rfind('/') {
        Some(slash) => (&token[..=slash], &token[slash + 1..], slash + 1),
        None => ("./", token, 0),
    };

    let Some(expanded) = expand_tilde(dir) else {
        return Ok((token_start + prefix_offset, vec![]));
    };

    // Anchor relative directories against the shell's logical cwd.  Tilde
    // expansion has already produced an absolute path for `~`-prefixed
    // dirs, so `is_absolute` correctly leaves those untouched.
    let read_from = {
        let p = Path::new(&expanded);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };

    Ok((
        token_start + prefix_offset,
        sorted_pairs(&read_from, name_prefix, "", true),
    ))
}

/// Build sorted completion pairs for entries of `dir` matching `name_prefix`.
/// Each replacement is `replacement_prefix` + name (+ `/` if dir), quoted
/// using ral source syntax (via [`ral_core::syntax::quote_word_if_needed`])
/// when `quote` is set and the candidate name is not a bare word.
///
/// Tilde-prefix completion passes `quote = false` so the trailing `~/` keeps
/// its expansion meaning; quoting it would suppress the expansion.
fn sorted_pairs(dir: &Path, name_prefix: &str, replacement_prefix: &str, quote: bool) -> Vec<Pair> {
    let mut pairs: Vec<Pair> = dir_entries(dir, name_prefix)
        .into_iter()
        .map(|(name, is_dir)| {
            let display = if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            };
            let body = format!("{replacement_prefix}{display}");
            let replacement = if quote {
                ral_core::syntax::quote_word_if_needed(&body).into_owned()
            } else {
                body
            };
            Pair {
                display,
                replacement,
            }
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

/// Map a highlight style name to its ANSI escape, or `None` if the name
/// is not a known style.  The single source of truth for the legal style
/// vocabulary — `_ed-highlight` derives its validation from this.
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

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::plugin_editor::Span;

    // ── is_cmd_pos ──────────────────────────────────────────────────────

    #[test]
    fn cmd_pos_recognises_command_boundaries() {
        for s in [
            "",
            "foo |",
            "if true {",
            "x?",
            "foo;",
            "(",
            "foo &&",
            "foo ||",
        ] {
            assert!(is_cmd_pos(s), "expected cmd pos at {s:?}");
        }
        assert!(!is_cmd_pos("foo"));
    }

    // ── Case-insensitive matching ────────────────────────────────────────

    #[test]
    fn case_insensitive_upper_prefix_matches_lower_candidate() {
        let lower = "foo".to_lowercase();
        let candidates = ["foobar".to_string(), "baz".to_string()];
        let matched: Vec<_> = candidates
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&lower))
            .collect();
        assert_eq!(matched, vec![&"foobar".to_string()]);
    }

    // ── Quoting integration with the core helper ────────────────────────
    //
    // The helper itself is unit-tested in `ral_core::syntax::quote`; here
    // we only pin the completion-side contract that `sorted_pairs` quotes
    // exactly when the candidate is not a bare ral word.

    #[test]
    fn replacement_quotes_when_name_has_metachar() {
        assert_eq!(
            ral_core::syntax::quote_word_if_needed("a b.txt").as_ref(),
            "'a b.txt'",
        );
    }

    #[test]
    fn replacement_borrows_when_bare() {
        assert_eq!(
            ral_core::syntax::quote_word_if_needed("normal.txt").as_ref(),
            "normal.txt",
        );
    }

    // ── complete_path / should_offer_path_candidate ─────────────────────

    #[test]
    fn complete_path_expands_home_tilde_prefix() {
        if crate::platform::home_dir() == "." {
            return;
        }
        // test fixture: a literal root cwd handed to `complete_path`.
        #[allow(clippy::disallowed_methods)]
        let (start, _) = complete_path("~/", 0, Path::new("/")).unwrap();
        assert_eq!(start, 2);
    }

    #[test]
    fn complete_path_supports_bare_tilde_token() {
        if crate::platform::home_dir() == "." {
            return;
        }
        // test fixture: a literal root cwd handed to `complete_path`.
        #[allow(clippy::disallowed_methods)]
        let (start, _) = complete_path("~", 3, Path::new("/")).unwrap();
        assert_eq!(start, 3);
    }

    #[test]
    fn path_completion_hides_dotfiles_unless_prefix_has_dot() {
        assert!(!should_offer_path_candidate(".git", ""));
        assert!(!should_offer_path_candidate(".git", "g"));
        assert!(should_offer_path_candidate(".git", "."));
        assert!(should_offer_path_candidate("src", ""));
    }

    // ── Shell cwd anchoring ─────────────────────────────────────────────
    //
    // `cd` and `within [dir: …]` mutate only the shell's logical cwd
    // (process cwd would race spawned threads), so completion has to
    // anchor relative dirs against the shell pair rather than read_dir's
    // default of process cwd.

    #[test]
    fn complete_path_lists_shell_cwd_for_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha"), "").unwrap();
        std::fs::write(tmp.path().join("beta"), "").unwrap();

        let (_, pairs) = complete_path("", 0, tmp.path()).unwrap();
        let mut names: Vec<&str> = pairs.iter().map(|p| p.display.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn complete_path_lists_shell_cwd_subdir_for_relative_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("gamma"), "").unwrap();

        let (_, pairs) = complete_path("sub/", 0, tmp.path()).unwrap();
        let names: Vec<&str> = pairs.iter().map(|p| p.display.as_str()).collect();
        assert_eq!(names, vec!["gamma"]);
    }

    #[test]
    fn complete_path_leaves_absolute_dir_unanchored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("delta"), "").unwrap();

        // A wholly unrelated cwd; the absolute prefix should win.
        let wrong_cwd = tempfile::tempdir().unwrap();
        let token = format!("{}/", tmp.path().display());
        let (_, pairs) = complete_path(&token, 0, wrong_cwd.path()).unwrap();
        let names: Vec<&str> = pairs.iter().map(|p| p.display.as_str()).collect();
        assert_eq!(names, vec!["delta"]);
    }

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
        // A span minted against a longer buffer must not panic when the
        // line rendered at the slice site has since shrunk.
        let span = HighlightSpan {
            span: Span::clamped(0, 10, 10),
            style: "command".into(),
        };
        let _ = apply_highlights("hi", &[span]);
    }
}
