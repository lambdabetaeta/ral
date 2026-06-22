//! Frontend-neutral tab/menu completion.
//!
//! The completion *engine*, owned by no frontend: it classifies the token
//! under the cursor (a `$`-variable, a command-position name, or a path),
//! gathers candidates from a [`Sources`] snapshot of the live shell, and
//! ranks them.  Both the rustyline helper ([`super::complete::RalHelper`])
//! and the structural surface's menu call [`complete`]; neither owns the
//! classification, the candidate sources, or the ranking.
//!
//! Ranking is fuzzy — the `nucleo` matcher, the Helix team's — for every
//! surface; [`rank`] is its single home.

use ral_core::Shell;
use std::path::{Path, PathBuf};

// ── Candidate / Sources ─────────────────────────────────────────────────────

/// One completion candidate: the text shown in a menu (`display`) and the
/// text substituted into the buffer when chosen (`replacement`).  A path
/// candidate's `replacement` is already source-quoted; a directory's
/// `display` ends in `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) display: String,
    pub(super) replacement: String,
}

/// A snapshot of the shell state completion draws on, rebuilt once per
/// prompt: the command names (PATH + builtins + handlers + bindings), the
/// `$`-variable names (bindings only), and the shell's logical cwd that path
/// completion anchors relative directories against.
///
/// `cd` and `within [dir: …]` mutate only the shell's logical cwd (the
/// process cwd would race spawned threads), so completion anchors relative
/// dirs against this pair rather than `read_dir`'s default of the process
/// cwd.
pub(super) struct Sources {
    pub(super) commands: Vec<String>,
    pub(super) variables: Vec<String>,
    pub(super) cwd: PathBuf,
}

impl Sources {
    /// Recompute completion state from the live shell.  Called once per
    /// prompt so new `let` bindings, `within [shell: PATH=…]` overrides, and
    /// `cd`-tracked cwd changes appear immediately.
    ///
    /// PATH lookup goes through the dynamic env overlay
    /// (`within [shell: PATH=…]` wins over the host) and through
    /// [`ral_core::path::commands_on_path`], which mirrors `locate`'s rules —
    /// relative entries are anchored against the shell's cwd and the
    /// executable bit is required.  Completion offers exactly the commands the
    /// shell will actually run.
    pub(super) fn from_shell(shell: &Shell) -> Self {
        let mut variables: Vec<String> = shell
            .bindings()
            .into_iter()
            .filter_map(|(name, _)| (!name.starts_with('_')).then_some(name))
            .collect();
        variables.sort();
        variables.dedup();

        let mut commands = variables.clone();
        let cwd = shell.cwd();
        if let Some(path) = shell.env_var("PATH") {
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
                .handler_names()
                .filter(|name| !name.starts_with('_'))
                .map(str::to_string),
        );

        commands.sort();
        commands.dedup();
        Sources {
            commands,
            variables,
            cwd,
        }
    }
}

// ── The entry point ──────────────────────────────────────────────────────────

/// Complete the token ending at byte offset `pos` in `line`.  Returns the
/// byte offset the replacement starts at (where a frontend splices the chosen
/// `replacement`) and the ranked candidates, best first.
pub(super) fn complete(line: &str, pos: usize, sources: &Sources) -> (usize, Vec<Candidate>) {
    let (start, kind) = CompletionKind::classify(&line[..pos]);
    match kind {
        CompletionKind::Variable { prefix } => (start, rank_names(&sources.variables, prefix)),
        CompletionKind::Command { prefix } => (start, rank_names(&sources.commands, prefix)),
        CompletionKind::Path { token } => complete_path(token, start, &sources.cwd),
    }
}

/// Filter and rank `names` against `needle`, mapping each survivor to a
/// name-replacement [`Candidate`].
fn rank_names(names: &[String], needle: &str) -> Vec<Candidate> {
    rank::matches(needle, names.iter().collect(), false)
        .into_iter()
        .map(|name| Candidate {
            display: name.clone(),
            replacement: name.clone(),
        })
        .collect()
}

// ── Classification ───────────────────────────────────────────────────────────

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

// ── Path completion ───────────────────────────────────────────────────────────

/// Expand a tilde-prefixed directory component for completion.  Delegates to
/// [`ral_core::path::tilde`] so the rule matches the rest of ral; returns
/// `None` when the home directory is unavailable so the caller can fall back
/// to non-tilde completion.
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

/// A directory entry offered as a path candidate.  Carries `is_dir` so the
/// display can append `/`, and exposes its name as the haystack [`rank`]
/// matches the needle against.
struct Entry {
    name: String,
    is_dir: bool,
}

impl AsRef<str> for Entry {
    fn as_ref(&self) -> &str {
        &self.name
    }
}

/// List the offerable entries of `dir` — those passing the dotfile gate
/// ([`dotfile_visible`]) — leaving the needle match to [`rank`].  Returns an
/// empty list when the directory cannot be read.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:complete-readdir] directory listing for tab-completion candidates; not turn-time model I/O"
)]
fn dir_entries(dir: &Path, needle: &str) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            dotfile_visible(&name, needle).then(|| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Entry { name, is_dir }
            })
        })
        .collect()
}

/// Whether `name` is offerable given the needle: a dotfile is hidden unless
/// the needle itself starts with `.`.  The match against the needle is
/// [`rank`]'s job; this is only the visibility gate.
pub(super) fn dotfile_visible(name: &str, needle: &str) -> bool {
    needle.starts_with('.') || !name.starts_with('.')
}

// Adapter site: turns user-typed completion tokens (a home dir, a
// tilde-expanded directory) into `&Path`s to read for candidates. These are
// the "user-input literal paths" adapter case clippy.toml sanctions for a
// local `Path::new` with a reason.
#[allow(clippy::disallowed_methods)]
pub(super) fn complete_path(
    token: &str,
    token_start: usize,
    cwd: &Path,
) -> (usize, Vec<Candidate>) {
    // Bare `~`: list home directory with `~/` prefix on replacements.  The
    // replacement must include `~/` because a frontend replaces from
    // `token_start`; quoting it would suppress tilde expansion, so names with
    // special chars are left bare on this path.
    if token == "~" {
        let home = crate::platform::home_dir();
        if home == "." {
            return (token_start, vec![]);
        }
        return (
            token_start,
            ranked_entries(Path::new(&home), "", "~/", false),
        );
    }

    // Split at last `/` to obtain the directory to read and the name needle.
    let (dir, name_needle, prefix_offset) = match token.rfind('/') {
        Some(slash) => (&token[..=slash], &token[slash + 1..], slash + 1),
        None => ("./", token, 0),
    };

    let Some(expanded) = expand_tilde(dir) else {
        return (token_start + prefix_offset, vec![]);
    };

    // Anchor relative directories against the shell's logical cwd.  Tilde
    // expansion has already produced an absolute path for `~`-prefixed dirs,
    // so `is_absolute` correctly leaves those untouched.
    let read_from = {
        let p = Path::new(&expanded);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };

    (
        token_start + prefix_offset,
        ranked_entries(&read_from, name_needle, "", true),
    )
}

/// Build ranked completion candidates for entries of `dir` matching
/// `name_needle`.  Each replacement is `replacement_prefix` + name (+ `/` if a
/// directory), quoted using ral source syntax (via
/// [`ral_core::syntax::quote_word_if_needed`]) when `quote` is set and the
/// candidate name is not a bare word.
///
/// Tilde-prefix completion passes `quote = false` so the trailing `~/` keeps
/// its expansion meaning; quoting it would suppress the expansion.
fn ranked_entries(
    dir: &Path,
    name_needle: &str,
    replacement_prefix: &str,
    quote: bool,
) -> Vec<Candidate> {
    rank::matches(name_needle, dir_entries(dir, name_needle), true)
        .into_iter()
        .map(|e| {
            let display = if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name.clone()
            };
            let body = format!("{replacement_prefix}{display}");
            let replacement = if quote {
                ral_core::syntax::quote_word_if_needed(&body).into_owned()
            } else {
                body
            };
            Candidate {
                display,
                replacement,
            }
        })
        .collect()
}

// ── Ranking ────────────────────────────────────────────────────────────────
//
// Fuzzy ranking via `nucleo`.  An empty needle returns every item (sorted), so
// an empty prefix lists everything.

mod rank {
    use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
    use nucleo_matcher::{Config, Matcher};

    /// Fuzzy-rank `items` against `needle`, best first, dropping non-matches.
    /// `paths` tunes the matcher for path-like haystacks (a `/`-aware boundary
    /// bonus).  Ties break alphabetically so the order is deterministic.
    pub(super) fn matches<T: AsRef<str>>(needle: &str, items: Vec<T>, paths: bool) -> Vec<T> {
        let config = if paths {
            Config::DEFAULT.match_paths()
        } else {
            Config::DEFAULT
        };
        let mut matcher = Matcher::new(config);
        let atom = Atom::new(
            needle,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );
        let mut scored = atom.match_list(items, &mut matcher);
        // `match_list` already orders by score descending (stable in input
        // order on ties); re-break ties alphabetically for a deterministic order.
        scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| a.as_ref().cmp(b.as_ref())));
        scored.into_iter().map(|(item, _)| item).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding"
)]
mod tests {
    use super::*;

    /// Build a `Sources` directly from name lists for the classification and
    /// ranking tests, with a throwaway cwd path completion never reads.
    fn sources(commands: &[&str], variables: &[&str]) -> Sources {
        Sources {
            commands: commands.iter().map(|s| s.to_string()).collect(),
            variables: variables.iter().map(|s| s.to_string()).collect(),
            cwd: PathBuf::from("/"),
        }
    }

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

    // ── complete: classification + matching ─────────────────────────────

    #[test]
    fn complete_offers_commands_at_command_position() {
        let src = sources(&["grep", "git", "ls"], &[]);
        let (start, cands) = complete("gi", 2, &src);
        assert_eq!(start, 0);
        assert!(cands.iter().any(|c| c.display == "git"));
    }

    #[test]
    fn complete_offers_variables_after_dollar() {
        let src = sources(&["echo"], &["dirs", "data"]);
        let (start, cands) = complete("echo $da", 8, &src);
        // The replacement starts after the `$`.
        assert_eq!(start, 6);
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"data"));
        // A `$`-completion never offers commands.
        assert!(!names.contains(&"echo"));
    }

    #[test]
    fn complete_is_case_insensitive() {
        let src = sources(&["Foobar", "baz"], &[]);
        let cands = complete("foo", 3, &src).1;
        assert!(cands.iter().any(|c| c.display == "Foobar"));
    }

    /// Fuzzy ranking accepts a non-prefix subsequence — the defining behaviour
    /// over plain prefix matching: `rgp` ⊂ `ripgrep`.
    #[test]
    fn complete_matches_fuzzy_subsequence() {
        let src = sources(&["ripgrep", "ls"], &[]);
        let cands = complete("rgp", 3, &src).1;
        assert!(
            cands.iter().any(|c| c.display == "ripgrep"),
            "fuzzy match should reach a non-prefix subsequence"
        );
    }

    /// A prefix match outranks a scattered subsequence: `gr` ranks `grep`
    /// (prefix) above `ripgrep` (gap match).
    #[test]
    fn complete_ranks_prefix_above_gap_match() {
        let src = sources(&["ripgrep", "grep"], &[]);
        let cands = complete("gr", 2, &src).1;
        let order: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(order.first(), Some(&"grep"), "got {order:?}");
    }

    // ── Quoting integration with the core helper ────────────────────────
    //
    // The helper itself is unit-tested in `ral_core::syntax::quote`; here we
    // only pin the completion-side contract that path candidates quote exactly
    // when the candidate is not a bare ral word.

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

    // ── complete_path / dotfile_visible ─────────────────────────────────

    #[test]
    fn complete_path_expands_home_tilde_prefix() {
        if crate::platform::home_dir() == "." {
            return;
        }
        let (start, _) = complete_path("~/", 0, Path::new("/"));
        assert_eq!(start, 2);
    }

    #[test]
    fn complete_path_supports_bare_tilde_token() {
        if crate::platform::home_dir() == "." {
            return;
        }
        let (start, _) = complete_path("~", 3, Path::new("/"));
        assert_eq!(start, 3);
    }

    #[test]
    fn dotfile_gate_hides_dotfiles_unless_needle_has_dot() {
        assert!(!dotfile_visible(".git", ""));
        assert!(!dotfile_visible(".git", "g"));
        assert!(dotfile_visible(".git", "."));
        assert!(dotfile_visible("src", ""));
    }

    // ── Shell cwd anchoring ─────────────────────────────────────────────

    #[test]
    fn complete_path_lists_shell_cwd_for_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha"), "").unwrap();
        std::fs::write(tmp.path().join("beta"), "").unwrap();

        let (_, cands) = complete_path("", 0, tmp.path());
        let mut names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn complete_path_lists_shell_cwd_subdir_for_relative_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("gamma"), "").unwrap();

        let (_, cands) = complete_path("sub/", 0, tmp.path());
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["gamma"]);
    }

    #[test]
    fn complete_path_leaves_absolute_dir_unanchored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("delta"), "").unwrap();

        // A wholly unrelated cwd; the absolute prefix should win.
        let wrong_cwd = tempfile::tempdir().unwrap();
        let token = format!("{}/", tmp.path().display());
        let (_, cands) = complete_path(&token, 0, wrong_cwd.path());
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["delta"]);
    }
}
