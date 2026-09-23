//! Frontend-neutral tab/menu completion.
//!
//! The completion *engine*, owned by no frontend: it classifies the token
//! under the cursor (a `$`-variable, a command-position name, or a path),
//! gathers candidates from a [`Sources`] view the engine's probes answer,
//! and ranks them.  Both the rustyline helper ([`super::complete::RalHelper`]) and the
//! structural surface's menu call [`complete`] against a shared
//! [`SourceCache`]; neither owns the classification, the candidate sources,
//! or the ranking.
//!
//! The sources are split in two by cost.  The cheap half — bindings,
//! builtins, handlers, the logical cwd — is recomputed once per prompt, so a
//! new `let` binding is offerable on the next line.  The expensive half is the
//! `PATH` enumeration, which lists every entry on the search list; that
//! one is lazy, taken on the first completion request that needs it and reused
//! until [`SCAN_TTL`] runs out or the search list moves.  A prompt therefore
//! reaches the screen without touching the disk, which where `PATH` holds
//! thousands of files is the difference between a prompt that appears and one
//! that arrives.
//!
//! Ranking is fuzzy — the `nucleo` matcher, the Helix team's — for every
//! surface; [`ral_core::text::rank`] is its single home.

use ral_core::protocol::Transport;
use ral_core::protocol::reading::{self, PathEntry};
use ral_core::text::rank;
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
/// pool, the `$`-variable names (bindings only), and the engine whose
/// filesystem path completion lists — relative directories anchored against
/// its logical cwd, since `cd` moves only that.
///
/// Borrowed from the [`SourceCache`] rather than owned, because the `PATH`
/// half runs to thousands of names and an owned merge would clone every one of
/// them per Tab.
pub(super) struct Sources<'a> {
    /// The executables reachable through the effective `PATH`.
    path_commands: &'a [String],
    /// Bindings, builtins and handlers — what costs no disk access to learn.
    shell_commands: &'a [String],
    variables: &'a [String],
    engine: &'a dyn Transport,
}

impl Sources<'_> {
    /// Every name offerable at command position, deduplicated across the two
    /// halves as well as within them: a binding name is also a command-position
    /// name, and the `PATH` scan repeats a name once per directory holding it.
    /// Only the references are sorted, and that order is not load-bearing —
    /// [`rank`] re-sorts by score.
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

/// The completion sources a frontend owns across prompts: the cheap engine
/// state, refreshed every prompt, and the lazily-taken `PATH` enumeration.
///
/// One type shared by both frontends, so the two cannot drift on the freshness
/// contract.
pub(super) struct SourceCache {
    shell_commands: Vec<String>,
    variables: Vec<String>,
    /// What the scan was taken against, captured at refresh.
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
#[derive(Default, PartialEq, Eq)]
struct PathKey {
    path: Option<String>,
    cwd: PathBuf,
}

/// One enumeration of `PATH`, stamped so [`SCAN_TTL`] can age it out.
struct PathScan {
    names: Vec<String>,
    taken: Instant,
}

/// The names not hidden behind a leading `_`.
fn public(names: Vec<String>) -> impl Iterator<Item = String> {
    names.into_iter().filter(|name| !name.starts_with('_'))
}

impl SourceCache {
    /// A cache that has touched no disk: constructing a frontend must not walk
    /// `PATH`, or startup pays the disk-walk cost each prompt otherwise avoids.
    pub(super) fn new() -> Self {
        Self {
            shell_commands: Vec::new(),
            variables: Vec::new(),
            key: PathKey::default(),
            scan: OnceCell::new(),
        }
    }

    /// Recompute the cheap completion state from the engine, and age the
    /// `PATH` scan.  Called once per prompt.
    pub(super) fn refresh(&mut self, engine: &dyn Transport) {
        self.refresh_at(engine, Instant::now());
    }

    /// [`SourceCache::refresh`] with the clock passed in, so [`SCAN_TTL`] is
    /// testable without sleeping through it.
    fn refresh_at(&mut self, engine: &dyn Transport, now: Instant) {
        // The cheap half, eagerly: no I/O, so "a new binding completes
        // immediately" stays true for free.  Holding it in its own fields is
        // also what makes it impossible for a binding change to invalidate a
        // `PATH` enumeration.
        let (bindings, handlers) = reading::completion_names(engine)
            .map_or_else(|_| Default::default(), |n| (n.bindings, n.handlers));
        self.variables = public(bindings).collect();
        let mut shell_commands: Vec<String> = self
            .variables
            .iter()
            .cloned()
            .chain(public(reading::builtin_names(engine).unwrap_or_default()))
            .chain(public(handlers))
            .collect();
        shell_commands.sort();
        shell_commands.dedup();
        self.shell_commands = shell_commands;

        // The expensive half is only invalidated here, never taken: a prompt
        // must not read a directory.  The search list comes through the dynamic
        // env overlay, so a `within [shell: PATH=…]` override keys differently
        // and drops the enclosing scope's answer.
        let key = PathKey {
            path: reading::env_var(engine, "PATH").ok().flatten(),
            cwd: reading::cwd(engine).unwrap_or_default(),
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
    /// against the engine's cwd, the executable bit required, and an empty
    /// `PATH` element dropped rather than read as the cwd, so a trailing `;`
    /// does not offer every file of the current directory as a command.
    /// Dispatch still goes through the fresh `locate`, so a scan gone stale can
    /// only misinform a menu, never misdirect a spawn.
    pub(super) fn sources<'a>(&'a self, engine: &'a dyn Transport) -> Sources<'a> {
        let scan = self.scan.get_or_init(|| PathScan {
            names: self
                .key
                .path
                .as_deref()
                .map(|path| {
                    std::env::split_paths(path)
                        .filter(|dir| !dir.as_os_str().is_empty())
                        .flat_map(|dir| entries(engine, &dir))
                        .filter_map(|e| e.exec.then_some(e.name))
                        .collect()
                })
                .unwrap_or_default(),
            taken: Instant::now(),
        });
        Sources {
            path_commands: &scan.names,
            shell_commands: &self.shell_commands,
            variables: &self.variables,
            engine,
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

/// `dir`'s entries in the engine's filesystem; none, if it will not say.
fn entries(engine: &dyn Transport, dir: &Path) -> Vec<PathEntry> {
    reading::path_entries(engine, dir).unwrap_or_default()
}

// ── The entry point ──────────────────────────────────────────────────────────

/// Complete the token ending at byte offset `pos` in `line`.  Returns the
/// byte offset the replacement starts at (where a frontend splices the chosen
/// `replacement`) and the ranked candidates, best first.
pub(super) fn complete(line: &str, pos: usize, sources: &Sources<'_>) -> (usize, Vec<Candidate>) {
    let (start, kind) = CompletionKind::classify(&line[..pos]);
    match kind {
        CompletionKind::Variable { prefix } => (
            start,
            rank_names(sources.variables.iter().collect(), prefix),
        ),
        CompletionKind::Command { prefix } => (start, rank_names(sources.command_names(), prefix)),
        CompletionKind::Path { token } => complete_path(token, start, sources.engine),
    }
}

/// Filter and rank `names` against `needle`, mapping each survivor to a
/// name-replacement [`Candidate`].  Generic over the borrow so the command pool
/// can be ranked as `&String`s gathered from two backing vectors, cloning
/// nothing until a name survives the match.
fn rank_names<T: AsRef<str>>(names: Vec<T>, needle: &str) -> Vec<Candidate> {
    rank(needle, names, false)
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
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: completion completes for the launching user, outside any shell overlay"
)]
fn expand_tilde(dir: &str) -> Option<String> {
    let Some(parsed) = ral_core::path::tilde::TildePath::parse(dir) else {
        return Some(dir.to_string());
    };
    let home = ral_core::host::home();
    ral_core::path::tilde::expand_tilde_path(
        parsed.user.as_deref(),
        parsed.suffix.as_deref(),
        home.as_deref(),
    )
    .ok()
}

/// A path entry as the haystack [`rank`] matches the needle against.
struct Named(PathEntry);

impl AsRef<str> for Named {
    fn as_ref(&self) -> &str {
        &self.0.name
    }
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
    engine: &dyn Transport,
) -> (usize, Vec<Candidate>) {
    // Bare `~`: list home directory with `~/` prefix on replacements.  The
    // replacement must include `~/` because a frontend replaces from
    // `token_start`; quoting it would suppress tilde expansion, so names with
    // special chars are left bare on this path.
    if token == "~" {
        let Some(home) = ral_core::host::home() else {
            return (token_start, vec![]);
        };
        return (
            token_start,
            ranked_entries(entries(engine, Path::new(&home)), "", "~/", false),
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

    // The engine anchors a relative directory against its logical cwd;
    // tilde expansion has already made a `~`-prefixed one absolute.
    (
        token_start + prefix_offset,
        ranked_entries(entries(engine, Path::new(&expanded)), name_needle, "", true),
    )
}

/// Build ranked completion candidates for the `entries` matching
/// `name_needle` and passing the dotfile gate.  Each replacement is `replacement_prefix` + name (+ `/` if a
/// directory), quoted using ral source syntax (via
/// [`ral_core::syntax::quote_word_if_needed`]) when `quote` is set and the
/// candidate name is not a bare word.
///
/// Tilde-prefix completion passes `quote = false` so the trailing `~/` keeps
/// its expansion meaning; quoting it would suppress the expansion.
fn ranked_entries(
    entries: Vec<PathEntry>,
    name_needle: &str,
    replacement_prefix: &str,
    quote: bool,
) -> Vec<Candidate> {
    let visible: Vec<Named> = entries
        .into_iter()
        .filter(|e| dotfile_visible(&e.name, name_needle))
        .map(Named)
        .collect();
    rank(name_needle, visible, true)
        .into_iter()
        .map(|Named(e)| {
            let display = if e.dir {
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

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;

    use ral_core::protocol::IdentityTransport;

    /// Backing storage for a hand-built [`Sources`], which borrows its lists.
    struct Fixture {
        path_commands: Vec<String>,
        shell_commands: Vec<String>,
        variables: Vec<String>,
        engine: IdentityTransport,
    }

    impl Fixture {
        fn view(&self) -> Sources<'_> {
            Sources {
                path_commands: &self.path_commands,
                shell_commands: &self.shell_commands,
                variables: &self.variables,
                engine: &self.engine,
            }
        }
    }

    /// Backing store for the classification and ranking tests, which care
    /// about neither the command split nor the engine: every name goes in
    /// the shell half, and path completion is never asked.
    fn sources(commands: &[&str], variables: &[&str]) -> Fixture {
        Fixture {
            path_commands: Vec::new(),
            shell_commands: commands.iter().map(ToString::to_string).collect(),
            variables: variables.iter().map(ToString::to_string).collect(),
            engine: engine_at("", Path::new("/")),
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
    // A real engine with a planted `PATH`, so the assertions run against the
    // enumeration it would actually do rather than a stub of it.

    /// Whatever the `path-entries` executable test demands: the `+x` bit on
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

    /// An engine whose `PATH` is exactly `path` and whose logical cwd is `cwd`.
    fn engine_at(path: &str, cwd: &Path) -> IdentityTransport {
        let mut attach =
            ral_core::protocol::Attach::new(crate::repl::TEST_TAG, cwd.into(), cwd.into());
        attach.env.push(("PATH".into(), path.into()));
        crate::repl::engine_at(&attach, |_| {})
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
        let engine = engine_at(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&engine);
        assert!(!cache.has_scanned(), "a prompt must not enumerate PATH");
        cache.refresh(&engine);
        assert!(!cache.has_scanned(), "nor must the next one");
    }

    #[test]
    fn a_completion_request_walks_path_once() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let engine = engine_at(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&engine);
        assert!(
            command_completions(&cache.sources(&engine), "plantedone")
                .contains(&"plantedone".into())
        );
        assert!(cache.has_scanned());

        // Same PATH and cwd, so the answer stands and the walk is not repeated.
        cache.refresh(&engine);
        assert!(cache.has_scanned(), "an unchanged key keeps the scan");
        assert!(
            command_completions(&cache.sources(&engine), "plantedone")
                .contains(&"plantedone".into())
        );
    }

    #[test]
    fn a_changed_path_override_is_not_answered_from_another_paths_scan() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        make_executable(&a.path().join("plantedone"));
        make_executable(&b.path().join("plantedtwo"));

        let mut cache = SourceCache::new();
        let first = engine_at(&as_path_value(a.path()), a.path());
        cache.refresh(&first);
        assert!(
            command_completions(&cache.sources(&first), "plantedone")
                .contains(&"plantedone".into())
        );

        // A `within [shell: PATH=…]` block asks a different question and
        // cannot be handed the enclosing scope's answer.
        let second = engine_at(&as_path_value(b.path()), a.path());
        cache.refresh(&second);
        assert!(!cache.has_scanned(), "a changed PATH drops the scan");
        let offered = command_completions(&cache.sources(&second), "planted");
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
        let first = engine_at("./bin", a.path());
        cache.refresh(&first);
        assert!(
            command_completions(&cache.sources(&first), "plantedone")
                .contains(&"plantedone".into())
        );

        // Same `PATH` string; only the anchor differs.
        let second = engine_at("./bin", b.path());
        cache.refresh(&second);
        assert!(!cache.has_scanned(), "a changed cwd drops the scan");
        let offered = command_completions(&cache.sources(&second), "planted");
        assert!(offered.contains(&"plantedtwo".into()), "got {offered:?}");
        assert!(!offered.contains(&"plantedone".into()), "got {offered:?}");
    }

    /// The split, in one assertion: a binding defined on the previous line
    /// completes on this one, and learning it cost no directory reads.
    #[test]
    fn a_new_binding_completes_without_walking_again() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let engine = engine_at(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&engine);
        assert!(
            command_completions(&cache.sources(&engine), "plantedone")
                .contains(&"plantedone".into())
        );

        crate::repl::run_line(&engine, "let newname = ()");
        cache.refresh(&engine);
        assert!(
            command_completions(&cache.sources(&engine), "newname").contains(&"newname".into())
        );
        assert!(
            cache.has_scanned(),
            "a binding change must not invalidate the PATH scan"
        );
    }

    #[test]
    fn an_aged_scan_is_dropped_at_the_next_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("plantedone"));
        let engine = engine_at(&as_path_value(tmp.path()), tmp.path());

        let mut cache = SourceCache::new();
        cache.refresh(&engine);
        let _ = cache.sources(&engine);
        assert!(cache.has_scanned());

        // Only the *prompt's* clock is injected, so the TTL is exercised
        // without sleeping through it; `taken` is no earlier than the scan's
        // own stamp, which is all the assertions below need.
        let taken = Instant::now();
        cache.refresh_at(&engine, taken + SCAN_TTL / 2);
        assert!(cache.has_scanned(), "inside the TTL the scan stands");
        // Past it, `cargo install foo` becomes offerable without a restart.
        cache.refresh_at(&engine, taken + SCAN_TTL);
        assert!(
            !cache.has_scanned(),
            "past the TTL the next request re-walks"
        );
    }

    /// The two halves dedup against each other, not merely within.
    #[test]
    fn a_name_in_both_halves_is_offered_once() {
        let src = Fixture {
            path_commands: vec!["dup".into(), "dup".into()],
            ..sources(&["dup"], &[])
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

    // ── complete_path / dotfile_visible ─────────────────────────────────

    #[test]
    fn complete_path_expands_home_tilde_prefix() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete_path("~/", 0, &engine_at("", Path::new("/")));
        assert_eq!(start, 2);
    }

    #[test]
    fn complete_path_supports_bare_tilde_token() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete_path("~", 3, &engine_at("", Path::new("/")));
        assert_eq!(start, 3);
    }

    #[test]
    fn dotfile_gate_hides_dotfiles_unless_needle_has_dot() {
        assert!(!dotfile_visible(".git", ""));
        assert!(!dotfile_visible(".git", "g"));
        assert!(dotfile_visible(".git", "."));
        assert!(dotfile_visible("src", ""));
    }

    // ── Engine cwd anchoring ────────────────────────────────────────────

    #[test]
    fn complete_path_lists_shell_cwd_for_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha"), "").unwrap();
        std::fs::write(tmp.path().join("beta"), "").unwrap();

        let (_, cands) = complete_path("", 0, &engine_at("", tmp.path()));
        let mut names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn complete_path_lists_shell_cwd_subdir_for_relative_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("gamma"), "").unwrap();

        let (_, cands) = complete_path("sub/", 0, &engine_at("", tmp.path()));
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["gamma"]);
    }

    /// A path candidate's `replacement` is source-quoted exactly when the name
    /// is not a bare ral word; its `display` never is, and a directory keeps
    /// its trailing slash on both.
    #[test]
    fn complete_path_quotes_replacement_but_not_display() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("plain.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let (_, cands) = complete_path("", 0, &engine_at("", tmp.path()));
        let replacement = |display: &str| {
            cands
                .iter()
                .find(|c| c.display == display)
                .unwrap_or_else(|| panic!("no candidate for {display}"))
                .replacement
                .as_str()
        };
        assert_eq!(replacement("a b.txt"), "'a b.txt'");
        assert_eq!(replacement("plain.txt"), "plain.txt");
        assert_eq!(replacement("sub/"), "sub/");
    }

    #[test]
    fn complete_path_leaves_absolute_dir_unanchored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("delta"), "").unwrap();

        // A wholly unrelated cwd; the absolute prefix should win.
        let wrong_cwd = tempfile::tempdir().unwrap();
        let token = format!("{}/", tmp.path().display());
        let (_, cands) = complete_path(&token, 0, &engine_at("", wrong_cwd.path()));
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["delta"]);
    }
}
