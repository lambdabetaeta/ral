//! Frontend-neutral tab/menu completion.
//!
//! The completion *engine*, owned by no frontend: it classifies the token
//! under the cursor (a `$`-variable, a command-position name, or a path),
//! gathers candidates from a [`Sources`] view of the live shell, and ranks
//! them.  Both the rustyline helper ([`super::complete::RalHelper`]) and the
//! structural surface's menu call [`complete`] against a shared
//! [`SourceCache`]; neither owns the classification, the candidate sources,
//! or the ranking.
//!
//! The sources are split in two by cost.  The cheap half — bindings,
//! builtins, handlers, the logical cwd — is recomputed once per prompt, so a
//! new `let` binding is offerable on the next line.  The expensive half is the
//! `PATH` enumeration, which `read_dir`s every entry on the search list; that
//! one is lazy, taken on the first completion request that needs it and reused
//! until [`SCAN_TTL`] runs out or the search list moves.  A prompt therefore
//! reaches the screen without touching the disk, which where `PATH` holds
//! thousands of files is the difference between a prompt that appears and one
//! that arrives.
//!
//! Ranking is fuzzy — the `nucleo` matcher, the Helix team's — for every
//! surface; [`rank`] is its single home.

use ral_core::Shell;
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// What one completion request draws on: the two halves of the command-name
/// pool, the `$`-variable names (bindings only), and the shell's logical cwd
/// that path completion anchors relative directories against.
///
/// Borrowed from the [`SourceCache`] rather than owned, because the `PATH`
/// half runs to thousands of names and an owned merge would clone every one of
/// them per Tab.
///
/// `cd` and `within [dir: …]` mutate only the shell's logical cwd (the
/// process cwd would race spawned threads), so completion anchors relative
/// dirs against this pair rather than `read_dir`'s default of the process
/// cwd.
pub(super) struct Sources<'a> {
    /// The executables reachable through the effective `PATH`.
    path_commands: &'a [String],
    /// Bindings, builtins and handlers — what costs no disk access to learn.
    shell_commands: &'a [String],
    variables: &'a [String],
    cwd: &'a Path,
}

impl Sources<'_> {
    /// Every name offerable at command position, deduplicated across the two
    /// halves as well as within them: a binding name is also a command-position
    /// name, and [`ral_core::path::commands_on_path`] repeats a name once per
    /// directory holding it.  Only the references are sorted, and that order is
    /// not load-bearing — [`rank::matches`] re-sorts by score.
    fn command_names(&self) -> Vec<&String> {
        let mut names: Vec<&String> = self
            .path_commands
            .iter()
            .chain(self.shell_commands.iter())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// How long an enumeration of `PATH` is trusted.
///
/// Not forever, because `cargo install foo` then `foo<Tab>` has to start
/// working without restarting the shell; not shorter, because the walk costs
/// hundreds of milliseconds on a large `PATH`.  Ageing is *checked* at a prompt
/// refresh and the re-walk happens at the next completion request, so the cost
/// lands on a Tab the user asked for rather than on a prompt they are waiting
/// for.
const SCAN_TTL: Duration = Duration::from_mins(1);

/// The completion sources a frontend owns across prompts: the cheap shell
/// state, refreshed every prompt, and the lazily-taken `PATH` enumeration.
///
/// One type shared by both frontends, so the two cannot drift on the freshness
/// contract.
pub(super) struct SourceCache {
    shell_commands: Vec<String>,
    variables: Vec<String>,
    cwd: PathBuf,
    /// What the scan was taken against, captured at refresh so
    /// [`SourceCache::sources`] needs no `&Shell`.
    key: PathKey,
    /// The `PATH` walk, absent until the first request that needs it.
    ///
    /// A [`OnceCell`] and not a `RefCell`: `get_or_init` hands back a plain
    /// reference tied to `&self`, so the borrowed [`Sources`] composes with no
    /// guard to keep alive and no `BorrowMutError` however the line editor
    /// chooses to call in.  Invalidation happens only under the `&mut self` of
    /// [`SourceCache::refresh_at`], which installs a fresh cell, so the cell's
    /// lack of a shared-reference reset costs nothing.
    scan: OnceCell<PathScan>,
}

/// Everything an enumeration of `PATH` depends on.
///
/// `path` is the whole effective search list, which makes a
/// `within [shell: PATH=…]` override correct by construction: a block with its
/// own list asks a different question and gets its own answer.  It is an
/// `Option` so that an unset `PATH` is a key of its own rather than a hole that
/// looks cold on every Tab.  `cwd` is here because the walk anchors relative
/// entries (`./bin`) against it.
///
/// There is no member for the Windows executable-suffix list, unlike the
/// dispatch memo's key: [`ral_core::path::commands_on_path`] never consults it,
/// listing every executable file in a directory rather than probing one name
/// against a set of suffixes.  (Naming that list's reader here would link a
/// `cfg(windows)`-only item from an unguarded doc, which fails elsewhere.)
#[derive(PartialEq, Eq)]
struct PathKey {
    path: Option<String>,
    cwd: PathBuf,
}

/// One enumeration of `PATH`, stamped so [`SCAN_TTL`] can age it out.
struct PathScan {
    names: Vec<String>,
    taken: Instant,
}

impl SourceCache {
    /// A cache that has touched no disk: constructing a frontend must not walk
    /// `PATH`, or startup pays what the prompt used to.
    pub(super) fn new() -> Self {
        Self {
            shell_commands: Vec::new(),
            variables: Vec::new(),
            cwd: PathBuf::new(),
            key: PathKey {
                path: None,
                cwd: PathBuf::new(),
            },
            scan: OnceCell::new(),
        }
    }

    /// Recompute the cheap completion state from the live shell, and age the
    /// `PATH` scan.  Called once per prompt.
    pub(super) fn refresh(&mut self, shell: &Shell) {
        self.refresh_at(shell, Instant::now());
    }

    /// [`SourceCache::refresh`] with the clock passed in, so [`SCAN_TTL`] is
    /// testable without sleeping through it.
    fn refresh_at(&mut self, shell: &Shell, now: Instant) {
        // The cheap half, eagerly: a scope fold and no I/O, so "a new binding
        // completes immediately" stays true for free.  Holding it in its own
        // fields is also what makes it impossible for a binding change to
        // invalidate a `PATH` enumeration.
        let mut variables: Vec<String> = shell
            .bindings()
            .into_iter()
            .filter_map(|(name, _)| (!name.starts_with('_')).then_some(name))
            .collect();
        variables.sort();
        variables.dedup();

        let mut shell_commands = variables.clone();
        shell_commands.extend(
            shell
                .builtin_names()
                .filter(|name| !name.starts_with('_'))
                .map(str::to_string),
        );
        shell_commands.extend(
            shell
                .handler_names()
                .filter(|name| !name.starts_with('_'))
                .map(str::to_string),
        );
        shell_commands.sort();
        shell_commands.dedup();

        self.variables = variables;
        self.shell_commands = shell_commands;
        self.cwd = shell.cwd();

        // The expensive half is only invalidated here, never taken: a prompt
        // must not read a directory.  The search list comes through the dynamic
        // env overlay, so a `within [shell: PATH=…]` override keys differently
        // and drops the enclosing scope's answer.
        let key = PathKey {
            path: shell.env_var("PATH"),
            cwd: self.cwd.clone(),
        };
        let aged = self
            .scan
            .get()
            .is_some_and(|scan| now.duration_since(scan.taken) >= SCAN_TTL);
        if key != self.key || aged {
            self.key = key;
            self.scan = OnceCell::new();
        }
    }

    /// The view one completion request ranks against, enumerating `PATH` if
    /// this is the first request since the scan was dropped.
    ///
    /// The enumeration mirrors `locate`'s rules — relative entries anchored
    /// against the shell's cwd, the executable bit required.  Dispatch still
    /// goes through the fresh `locate`, so a scan gone stale can only misinform
    /// a menu, never misdirect a spawn.
    pub(super) fn sources(&self) -> Sources<'_> {
        let scan = self.scan.get_or_init(|| PathScan {
            names: self
                .key
                .path
                .as_deref()
                .map(|path| ral_core::path::commands_on_path(path, Some(&self.cwd)))
                .unwrap_or_default(),
            taken: Instant::now(),
        });
        Sources {
            path_commands: &scan.names,
            shell_commands: &self.shell_commands,
            variables: &self.variables,
            cwd: &self.cwd,
        }
    }

    /// Whether the `PATH` walk has been paid since the scan was last dropped.
    /// The cell's occupancy *is* the enumeration counter, so a test can assert
    /// "this read no directories" without timing anything and without
    /// instrumentation in the shipped binary.
    #[cfg(test)]
    fn has_scanned(&self) -> bool {
        self.scan.get().is_some()
    }
}

// ── The entry point ──────────────────────────────────────────────────────────

/// Complete the token ending at byte offset `pos` in `line`.  Returns the
/// byte offset the replacement starts at (where a frontend splices the chosen
/// `replacement`) and the ranked candidates, best first.
pub(super) fn complete(line: &str, pos: usize, sources: &Sources<'_>) -> (usize, Vec<Candidate>) {
    let (start, kind) = CompletionKind::classify(&line[..pos]);
    match kind {
        CompletionKind::Variable { prefix } => {
            (start, rank_names(sources.variables.iter().collect(), prefix))
        }
        CompletionKind::Command { prefix } => (start, rank_names(sources.command_names(), prefix)),
        CompletionKind::Path { token } => complete_path(token, start, sources.cwd),
    }
}

/// Filter and rank `names` against `needle`, mapping each survivor to a
/// name-replacement [`Candidate`].  Generic over the borrow so the command pool
/// can be ranked as `&String`s gathered from two backing vectors, cloning
/// nothing until a name survives the match.
fn rank_names<T: AsRef<str>>(names: Vec<T>, needle: &str) -> Vec<Candidate> {
    rank::matches(needle, names, false)
        .into_iter()
        .map(|name| Candidate {
            display: name.as_ref().to_owned(),
            replacement: name.as_ref().to_owned(),
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
/// `None` when the home directory is unavailable, or when the component
/// names another user's home and this platform has no way to look one up
/// (no `getpwnam(3)` equivalent) — either way the caller offers no
/// candidates rather than completing against a fabricated path.
fn expand_tilde(dir: &str) -> Option<String> {
    let Some(parsed) = ral_core::path::tilde::TildePath::parse(dir) else {
        return Some(dir.to_string());
    };
    let home = crate::platform::home_dir();
    if home == "." {
        return None;
    }
    ral_core::path::tilde::expand_tilde_path(
        parsed.user.as_deref(),
        parsed.suffix.as_deref(),
        &home,
    )
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
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                Entry { name, is_dir }
            })
        })
        .collect()
}

/// Whether `name` is offerable given the needle: a dotfile is hidden unless
/// the needle itself starts with `.`.  The match against the needle is
/// [`rank`]'s job; this is only the visibility gate.
fn dotfile_visible(name: &str, needle: &str) -> bool {
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
                e.name
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

    /// Backing storage for a hand-built [`Sources`], which borrows its lists.
    struct Fixture {
        path_commands: Vec<String>,
        shell_commands: Vec<String>,
        variables: Vec<String>,
        cwd: PathBuf,
    }

    impl Fixture {
        fn view(&self) -> Sources<'_> {
            Sources {
                path_commands: &self.path_commands,
                shell_commands: &self.shell_commands,
                variables: &self.variables,
                cwd: &self.cwd,
            }
        }
    }

    /// Backing store for the classification and ranking tests, which care
    /// about neither the command split nor the cwd: every name goes in the
    /// shell half and the cwd is a throwaway path completion never reads.
    fn sources(commands: &[&str], variables: &[&str]) -> Fixture {
        Fixture {
            path_commands: Vec::new(),
            shell_commands: commands.iter().map(ToString::to_string).collect(),
            variables: variables.iter().map(ToString::to_string).collect(),
            cwd: PathBuf::from("/"),
        }
    }

    /// The names a command-position completion of `needle` offers.
    fn command_completions(src: &Sources<'_>, needle: &str) -> Vec<String> {
        complete(needle, needle.len(), src)
            .1
            .into_iter()
            .map(|c| c.display)
            .collect()
    }

    // ── Cache fixtures ──────────────────────────────────────────────────
    //
    // A real `Shell` with a planted `PATH`, so the assertions run against the
    // enumeration the shell would actually do rather than a stub of it.

    /// Whatever `commands_on_path`'s executable test demands: the `+x` bit on
    /// Unix, nothing off it.
    fn make_executable(p: &Path) {
        std::fs::write(p, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(p, perms).unwrap();
        }
    }

    /// A shell whose `PATH` is exactly `path` and whose logical cwd is `cwd`.
    fn shell_with(path: &str, cwd: &Path) -> Shell {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        shell.seed_cwd(cwd.to_path_buf());
        shell.set_env_var("PATH", path);
        shell
    }

    fn as_path_value(dir: &Path) -> String {
        dir.to_str().unwrap().to_owned()
    }

    // ── The lazy PATH scan ──────────────────────────────────────────────

    /// The regression test for the slow prompt: pre-prompt housekeeping must
    /// not read a single directory, however many prompts it runs for.
    #[test]
    fn a_prompt_refresh_does_not_walk_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let shell = shell_with(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&shell);
        assert!(!cache.has_scanned(), "a prompt must not enumerate PATH");
        cache.refresh(&shell);
        assert!(!cache.has_scanned(), "nor must the next one");
    }

    #[test]
    fn a_completion_request_walks_path_once() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let shell = shell_with(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&shell);
        assert!(command_completions(&cache.sources(), "plantedone").contains(&"plantedone".into()));
        assert!(cache.has_scanned());

        // Same PATH and cwd, so the answer stands and the walk is not repeated.
        cache.refresh(&shell);
        assert!(cache.has_scanned(), "an unchanged key keeps the scan");
        assert!(command_completions(&cache.sources(), "plantedone").contains(&"plantedone".into()));
    }

    #[test]
    fn a_changed_path_override_is_not_answered_from_another_paths_scan() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        make_executable(&a.path().join("plantedone"));
        make_executable(&b.path().join("plantedtwo"));

        let mut cache = SourceCache::new();
        cache.refresh(&shell_with(&as_path_value(a.path()), a.path()));
        assert!(command_completions(&cache.sources(), "plantedone").contains(&"plantedone".into()));

        // A `within [shell: PATH=…]` block asks a different question and
        // cannot be handed the enclosing scope's answer.
        cache.refresh(&shell_with(&as_path_value(b.path()), a.path()));
        assert!(!cache.has_scanned(), "a changed PATH drops the scan");
        let offered = command_completions(&cache.sources(), "planted");
        assert!(offered.contains(&"plantedtwo".into()), "got {offered:?}");
        assert!(!offered.contains(&"plantedone".into()), "got {offered:?}");
    }

    #[test]
    fn a_changed_cwd_re_anchors_a_relative_entry() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for root in [a.path(), b.path()] {
            std::fs::create_dir(root.join("bin")).unwrap();
        }
        make_executable(&a.path().join("bin").join("plantedone"));
        make_executable(&b.path().join("bin").join("plantedtwo"));

        let mut cache = SourceCache::new();
        cache.refresh(&shell_with("./bin", a.path()));
        assert!(command_completions(&cache.sources(), "plantedone").contains(&"plantedone".into()));

        // Same `PATH` string; only the anchor differs.
        cache.refresh(&shell_with("./bin", b.path()));
        assert!(!cache.has_scanned(), "a changed cwd drops the scan");
        let offered = command_completions(&cache.sources(), "planted");
        assert!(offered.contains(&"plantedtwo".into()), "got {offered:?}");
        assert!(!offered.contains(&"plantedone".into()), "got {offered:?}");
    }

    /// The split, in one assertion: a binding defined on the previous line
    /// completes on this one, and learning it cost no directory reads.
    #[test]
    fn a_new_binding_completes_without_walking_again() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let mut shell = shell_with(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&shell);
        assert!(command_completions(&cache.sources(), "plantedone").contains(&"plantedone".into()));

        shell.set_var("newname".to_string(), ral_core::types::Value::Unit);
        cache.refresh(&shell);
        assert!(command_completions(&cache.sources(), "newname").contains(&"newname".into()));
        assert!(
            cache.has_scanned(),
            "a binding change must not invalidate the PATH scan"
        );
    }

    #[test]
    fn an_aged_scan_is_dropped_at_the_next_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let shell = shell_with(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&shell);
        let _ = cache.sources();
        assert!(cache.has_scanned());

        // Only the *prompt's* clock is injected, so the TTL is exercised
        // without sleeping through it; `taken` is no earlier than the scan's
        // own stamp, which is all the assertions below need.
        let taken = Instant::now();
        cache.refresh_at(&shell, taken + SCAN_TTL / 2);
        assert!(cache.has_scanned(), "inside the TTL the scan stands");
        // Past it, `cargo install foo` becomes offerable without a restart.
        cache.refresh_at(&shell, taken + SCAN_TTL);
        assert!(!cache.has_scanned(), "past the TTL the next request re-walks");
    }

    /// The two halves dedup against each other, not merely within.
    #[test]
    fn a_name_in_both_halves_is_offered_once() {
        let src = Fixture {
            path_commands: vec!["dup".into(), "dup".into()],
            shell_commands: vec!["dup".into()],
            variables: Vec::new(),
            cwd: PathBuf::from("/"),
        };
        let offered = command_completions(&src.view(), "dup");
        assert_eq!(offered, vec!["dup".to_string()]);
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
        let (start, cands) = complete("gi", 2, &src.view());
        assert_eq!(start, 0);
        assert!(cands.iter().any(|c| c.display == "git"));
    }

    #[test]
    fn complete_offers_variables_after_dollar() {
        let src = sources(&["echo"], &["dirs", "data"]);
        let (start, cands) = complete("echo $da", 8, &src.view());
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
        let cands = complete("foo", 3, &src.view()).1;
        assert!(cands.iter().any(|c| c.display == "Foobar"));
    }

    /// Fuzzy ranking accepts a non-prefix subsequence — the defining behaviour
    /// over plain prefix matching: `rgp` ⊂ `ripgrep`.
    #[test]
    fn complete_matches_fuzzy_subsequence() {
        let src = sources(&["ripgrep", "ls"], &[]);
        let cands = complete("rgp", 3, &src.view()).1;
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
        let cands = complete("gr", 2, &src.view()).1;
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
        names.sort_unstable();
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
