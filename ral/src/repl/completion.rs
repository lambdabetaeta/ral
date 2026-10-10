//! Frontend-neutral tab/menu completion.
//!
//! The completion *engine*, owned by no frontend: it classifies the token
//! under the cursor (a `$`-variable, a command-position name, or a path),
//! gathers candidates from a [`Sources`] view the engine's probes answer,
//! and ranks them.  The rustyline helper ([`super::complete::RalHelper`]) calls [`complete`] against a shared
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

use ral_core::carrier::Transport;
use ral_core::protocol::probe::PathEntry;
use ral_core::syntax::lexer::{Token, lex_open, separates};
use ral_core::syntax::quote_word;
use ral_core::text::rank;
use std::borrow::Cow;
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

/// What one completion request draws on: the [`SourceCache`] and the engine
/// whose filesystem path completion lists — relative directories anchored
/// against its logical cwd, since `cd` moves only that.
///
/// Borrowed rather than owned, because the `PATH` half runs to thousands of
/// names and an owned merge would clone every one of them per Tab.
pub(super) struct Sources<'a> {
    cache: &'a SourceCache,
    engine: &'a dyn Transport,
}

impl Sources<'_> {
    /// Every name offerable at command position, deduplicated across the two
    /// halves as well as within them: a binding name is also a command-position
    /// name, and the `PATH` scan repeats a name once per directory holding it.
    /// Only the references are sorted, and that order is not load-bearing —
    /// [`rank`] re-sorts by score.
    ///
    /// The one place `PATH` is enumerated, if this is the first request since
    /// the scan was dropped.  The enumeration mirrors `locate`'s rules —
    /// relative entries anchored against the engine's cwd, the executable bit
    /// required, and an empty `PATH` element dropped rather than read as the
    /// cwd, so a trailing `;` does not offer every file of the current
    /// directory as a command.  Dispatch still goes through the fresh
    /// `locate`, so a scan gone stale can only misinform a menu, never
    /// misdirect a spawn.
    fn command_names(&self) -> Vec<&String> {
        let scan = self.cache.scan.get_or_init(|| PathScan {
            names: self
                .cache
                .key
                .path
                .as_deref()
                .map(|path| {
                    std::env::split_paths(path)
                        .filter(|dir| !dir.as_os_str().is_empty())
                        .flat_map(|dir| entries(self.engine, &dir))
                        .filter_map(|e| e.exec.then_some(e.name))
                        .collect()
                })
                .unwrap_or_default(),
            taken: Instant::now(),
        });
        let mut names: Vec<&String> = scan
            .names
            .iter()
            .chain(&self.cache.shell_commands)
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
        let (bindings, handlers) = engine
            .completion_names()
            .map_or_else(|_| Default::default(), |n| (n.bindings, n.handlers));
        self.variables = public(bindings).collect();
        let mut shell_commands: Vec<String> = self
            .variables
            .iter()
            .cloned()
            .chain(public(engine.builtin_names().unwrap_or_default()))
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
            path: engine.env_var("PATH").ok().flatten(),
            cwd: engine.cwd().unwrap_or_default(),
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
    /// The view one completion request ranks against.  Reads nothing: the
    /// `PATH` walk waits for [`Sources::command_names`].
    pub(super) fn sources<'a>(&'a self, engine: &'a dyn Transport) -> Sources<'a> {
        Sources {
            cache: self,
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
    engine.path_entries(dir).unwrap_or_default()
}

// ── The entry point ──────────────────────────────────────────────────────────

/// Complete the atom under the cursor at byte offset `pos` in `line`.  Returns
/// the byte offset the replacement starts at (where a frontend splices the
/// chosen `replacement`) and the ranked candidates, best first.
///
/// Classification reads the lexer's own tokens of `line[..pos]`: the atom is
/// the last lexeme touching `pos`, and it is at command position when what
/// precedes it is nothing or a separator.  A path candidate replaces the whole
/// atom, so it is one atom again — words that touch do not parse.
#[allow(
    clippy::disallowed_methods,
    reason = "adapter: the directory part of a user-typed token, read for candidates"
)]
pub(super) fn complete(line: &str, pos: usize, sources: &Sources<'_>) -> (usize, Vec<Candidate>) {
    let Some(lexemes) = lex_open(&line[..pos]) else {
        return (pos, vec![]);
    };
    // Lexemes starting at `pos` are the closers `lex_open` appended, or `Eof`.
    let seen = &lexemes[..lexemes.partition_point(|l| (l.span.start as usize) < pos)];
    let atom = seen
        .last()
        .filter(|l| l.span.end as usize >= pos && !separates(&l.token));
    let at_command = seen
        .len()
        .checked_sub(1 + usize::from(atom.is_some()))
        .is_none_or(|i| separates(&seen[i].token));
    // A quoted atom does not expand `~`.
    let (start, text, tilde) = match atom.map(|l| (&l.token, l.span)) {
        None => (pos, String::new(), false),
        Some((Token::Word(_), span)) => (
            span.start as usize,
            match &line[span.start as usize..span.end as usize] {
                "~" => "~/".into(), // a lone `~` is home as a directory
                word => word.to_owned(),
            },
            true,
        ),
        Some((Token::SingleQuoted(body), span)) => (span.start as usize, body.clone(), false),
        Some((Token::Variable { name, delimited }, span)) => {
            return (
                span.start as usize + 1 + usize::from(*delimited),
                rank_names(sources.cache.variables.iter().collect(), name),
            );
        }
        Some(_) => return (pos, vec![]),
    };
    if at_command && !text.contains('/') {
        return (start, rank_names(sources.command_names(), &text));
    }
    let (dir, needle) = text.split_at(text.rfind('/').map_or(0, |slash| slash + 1));
    let read = if dir.is_empty() { "./" } else { dir };
    let Some(read) = (!tilde)
        .then(|| read.to_owned())
        .or_else(|| expand_tilde(read))
    else {
        return (start, vec![]);
    };
    // The engine anchors a relative directory against its logical cwd.
    (
        start,
        ranked_entries(
            entries(sources.engine, Path::new(&read)),
            needle,
            dir,
            tilde,
        ),
    )
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

// ── Path completion ───────────────────────────────────────────────────────────

/// `dir` with a leading `~` expanded; `None` when no home is known, so the
/// caller offers no candidates rather than a fabricated path.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: completion completes for the launching user, outside any shell overlay"
)]
fn expand_tilde(dir: &str) -> Option<String> {
    ral_core::path::tilde::TildePath::parse(dir).map_or_else(
        || Some(dir.to_string()),
        |t| ral_core::host::home().map(|home| t.expand(&home)),
    )
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

/// Build ranked completion candidates for the `entries` matching `needle` and
/// passing the dotfile gate.  Each replacement is the whole path `dir` + name
/// (+ `/` if a directory) as one source atom, quoted by
/// [`ral_core::syntax::quote_word`] unless a bare word.  A `tilde` path that
/// needs quoting is expanded first, since a quoted `~` would not expand.
fn ranked_entries(entries: Vec<PathEntry>, needle: &str, dir: &str, tilde: bool) -> Vec<Candidate> {
    let visible: Vec<Named> = entries
        .into_iter()
        .filter(|e| dotfile_visible(&e.name, needle))
        .map(Named)
        .collect();
    rank(needle, visible, true)
        .into_iter()
        .map(|Named(e)| {
            let display = if e.dir {
                format!("{}/", e.name)
            } else {
                e.name
            };
            let full = format!("{dir}{display}");
            let full = if tilde && matches!(quote_word(&full), Cow::Owned(_)) {
                expand_tilde(&full).unwrap_or(full)
            } else {
                full
            };
            Candidate {
                replacement: quote_word(&full).into_owned(),
                display,
            }
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;

    use ral_core::carrier::IdentityTransport;

    /// Backing storage for a hand-built [`Sources`], which borrows its cache.
    struct Fixture {
        cache: SourceCache,
        engine: IdentityTransport,
    }

    impl Fixture {
        fn view(&self) -> Sources<'_> {
            self.cache.sources(&self.engine)
        }
    }

    /// Backing store for the classification and ranking tests: every name goes
    /// in the shell half, the `PATH` scan is empty, and the engine sits at `/`.
    fn sources(commands: &[&str], variables: &[&str]) -> Fixture {
        Fixture {
            cache: SourceCache {
                shell_commands: commands.iter().map(ToString::to_string).collect(),
                variables: variables.iter().map(ToString::to_string).collect(),
                key: PathKey::default(),
                scan: OnceCell::new(),
            },
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
        let _ = cache.sources(&engine).command_names();
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
        let mut src = sources(&["dup"], &[]);
        src.cache.scan = OnceCell::from(PathScan {
            names: vec!["dup".into(), "dup".into()],
            taken: Instant::now(),
        });
        let offered = command_completions(&src.view(), "dup");
        assert_eq!(offered, vec!["dup".to_string()]);
    }

    /// `$var` and path completion read no directory of `PATH`.
    #[test]
    fn variable_and_path_completion_do_not_walk_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha"), "").unwrap();
        let engine = engine_at(&as_path_value(tmp.path()), tmp.path());
        let mut cache = SourceCache::new();
        cache.refresh(&engine);
        for line in ["echo $da", "ls al", "ls ./al", "ls 'al"] {
            complete(line, line.len(), &cache.sources(&engine));
            assert!(!cache.has_scanned(), "{line:?} walked PATH");
        }
        complete("al", 2, &cache.sources(&engine));
        assert!(cache.has_scanned());
    }

    // ── complete: classification + matching ─────────────────────────────

    /// Command position is nothing before the atom, or a separator.
    #[test]
    fn complete_offers_commands_after_separators() {
        let src = sources(&["git"], &[]);
        for (line, start) in [("foo | gi", 6), ("if true { gi", 10), ("foo; gi", 5)] {
            let (at, cands) = complete(line, line.len(), &src.view());
            assert_eq!(at, start, "{line:?}");
            assert!(cands.iter().any(|c| c.display == "git"), "{line:?}");
        }
        let (_, cands) = complete("foo gi", 6, &src.view());
        assert!(cands.is_empty(), "an argument is a path");
    }

    /// The lexer reads U+2003 as a word character, so the atom starts at 0;
    /// no byte arithmetic may split the multibyte character.
    #[test]
    fn complete_after_an_em_space_does_not_panic() {
        let src = sources(&["git"], &[]);
        let line = "\u{2003}gi";
        let (start, _) = complete(line, line.len(), &src.view());
        assert_eq!(start, 0);
    }

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

    // ── path completion / dotfile_visible ───────────────────────────────

    /// A fixture whose engine sits in `cwd`.
    fn at(cwd: &Path) -> Fixture {
        Fixture {
            engine: engine_at("", cwd),
            ..sources(&[], &[])
        }
    }

    #[test]
    fn path_completion_expands_home_tilde_prefix() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete("ls ~/", 5, &at(Path::new("/")).view());
        assert_eq!(start, 3);
    }

    /// A lone `~` lists home, as the directory `~/`.
    #[test]
    fn path_completion_of_a_lone_tilde_lists_home() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete("ls ~", 4, &at(Path::new("/")).view());
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
    fn path_completion_lists_shell_cwd_for_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha"), "").unwrap();
        std::fs::write(tmp.path().join("beta"), "").unwrap();

        let (start, cands) = complete("ls ", 3, &at(tmp.path()).view());
        let mut names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        names.sort_unstable();
        assert_eq!((start, names), (3, vec!["alpha", "beta"]));
    }

    #[test]
    fn path_completion_lists_shell_cwd_subdir_for_relative_token() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("gamma"), "").unwrap();

        let (start, cands) = complete("ls sub/", 7, &at(tmp.path()).view());
        assert_eq!(start, 3);
        assert_eq!(cands.len(), 1);
        assert_eq!(
            (cands[0].display.as_str(), cands[0].replacement.as_str()),
            ("gamma", "sub/gamma")
        );
    }

    /// A path candidate's `replacement` is source-quoted exactly when the path
    /// is not a bare ral word; its `display` never is, and a directory keeps
    /// its trailing slash on both.
    #[test]
    fn path_completion_quotes_replacement_but_not_display() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("plain.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let (_, cands) = complete("ls ", 3, &at(tmp.path()).view());
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

    /// Inside a subdirectory the replacement is one quoted atom from the
    /// atom's start: `dir/'a b'` would not parse.
    #[test]
    fn path_completion_in_a_subdirectory_quotes_the_whole_atom() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("dir").join("a b"), "").unwrap();

        let (start, cands) = complete("ls dir/a", 8, &at(tmp.path()).view());
        assert_eq!(start, 3);
        assert_eq!(cands[0].replacement, "'dir/a b'");
    }

    /// An open quote is one atom, spaces included; the replacement closes it.
    #[test]
    fn path_completion_continues_an_open_single_quote() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("my dir")).unwrap();

        let (start, cands) = complete("ls 'my di", 9, &at(tmp.path()).view());
        assert_eq!(start, 3);
        assert_eq!(cands[0].replacement, "'my dir/'");
    }

    #[test]
    fn path_completion_leaves_absolute_dir_unanchored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("delta"), "").unwrap();

        // A wholly unrelated cwd; the absolute prefix should win.
        let wrong_cwd = tempfile::tempdir().unwrap();
        let line = format!("ls {}/", tmp.path().display());
        let (_, cands) = complete(&line, line.len(), &at(wrong_cwd.path()).view());
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["delta"]);
    }
}
