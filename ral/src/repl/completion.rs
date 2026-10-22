//! Frontend-neutral tab/menu completion.
//!
//! The completion *engine*, owned by no frontend: it classifies the token
//! under the cursor (a `$`-variable, a command-position name, or a path),
//! gathers candidates from a [`Sources`] view the engine's probes answer,
//! and matches them.  The rustyline helper ([`super::complete::RalHelper`]) calls [`complete`] against a shared
//! [`SourceCache`]; neither owns the classification, the candidate sources,
//! or the matching.
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
//! A request gathers one pool of names and [`matching`] runs once over it:
//! names extending the typed text, alphabetically, else fuzzy matches.

use ral_core::carrier::Transport;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::{Datum, exact_keys, field};
use ral_core::path::complete::{PartialPath, Tilde, spell_entry};
use ral_core::protocol::probe::PathEntry;
use ral_core::record;
use ral_core::source::Span as ByteSpan;
use ral_core::syntax::lexer::{Lexeme, Token, lex_open, separates};
use ral_core::syntax::quote_word;
use ral_core::text::{byte_to_char, char_to_byte, rank};
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

/// A bare string is both fields; otherwise a `[replacement, display]` record.
impl Datum for Candidate {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: vec![
                ("display".into(), self.display.encode()),
                ("replacement".into(), self.replacement.encode()),
            ],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        if let Some(s) = v.as_str() {
            return Ok(Self {
                display: s.into(),
                replacement: s.into(),
            });
        }
        exact_keys(v, &["replacement", "display"])?;
        Ok(Self {
            display: field(v, "display")?,
            replacement: field(v, "replacement")?,
        })
    }
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
    /// [`matching`] re-sorts.
    ///
    /// The one place `PATH` is enumerated, if this is the first request since
    /// the scan was dropped.  It walks `PATH`'s entries as written, each
    /// anchored by the engine, and requires the executable bit.  Dispatch still goes through the fresh
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
                    ral_core::path::path_entries(std::ffi::OsStr::new(path))
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
/// chosen `replacement`) and the matching candidates.
///
/// Classification reads the lexer's own tokens of `line[..pos]`: the atom is
/// the last lexeme touching `pos`, and it is at command position when what
/// precedes it is nothing or a separator.  A path candidate replaces the whole
/// atom, so it is one atom again — words that touch do not parse.
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
    let home = home();
    // A quoted atom does not expand `~`.
    let (start, partial) = match atom.map(|l| (&l.token, l.span)) {
        None => (pos, PartialPath::of("", Tilde::Literal)),
        Some((Token::Word(_), span)) => (
            span.start as usize,
            PartialPath::of(
                &line[span.start as usize..span.end as usize],
                Tilde::Home(home.as_deref()),
            ),
        ),
        Some((Token::SingleQuoted(body), span)) => {
            (span.start as usize, PartialPath::of(body, Tilde::Literal))
        }
        Some((Token::Variable { name, delimited }, span)) => {
            let pool = sources.cache.variables.iter().map(|v| Item::Name(v));
            return (
                span.start as usize + 1 + usize::from(*delimited),
                offer(name, pool),
            );
        }
        Some(_) => return (pos, vec![]),
    };
    let Some(listing) = partial.listing() else {
        return (start, vec![]);
    };
    // The engine anchors a relative directory against its logical cwd.
    let found = entries(sources.engine, &listing);
    let command = at_command && partial.is_bare();
    // At command position, only a directory: it may lead to an executable (`dir/cmd`).
    let paths = found
        .iter()
        .filter(|e| partial.offers(&e.name) && (e.dir || !command))
        .map(|entry| Item::Path {
            partial: &partial,
            entry,
        });
    let names = if command {
        sources.command_names()
    } else {
        Vec::new()
    };
    let pool = names.iter().map(|n| Item::Name(n)).chain(paths);
    (start, offer(partial.needle(), pool))
}

/// The launching user's home, for a typed `~`.
#[allow(
    clippy::disallowed_methods,
    reason = "host-env: completion completes for the launching user, outside any shell overlay"
)]
fn home() -> Option<String> {
    ral_core::host::home()
}

/// One offerable name, borrowed from the pool it was gathered from.
enum Item<'a> {
    Name(&'a str),
    Path {
        partial: &'a PartialPath<'a>,
        entry: &'a PathEntry,
    },
}

impl AsRef<str> for Item<'_> {
    fn as_ref(&self) -> &str {
        match self {
            Item::Name(n) => n,
            Item::Path { entry, .. } => &entry.name,
        }
    }
}

impl Item<'_> {
    /// A path replaces the whole atom, [`PartialPath::completed`], quoted
    /// unless a bare word; one that needs quoting has its `~` expanded first,
    /// since a quoted `~` would not expand.
    fn candidate(self) -> Candidate {
        match self {
            Item::Name(n) => Candidate {
                display: n.to_owned(),
                replacement: n.to_owned(),
            },
            Item::Path { partial, entry } => {
                let display = spell_entry(&entry.name, entry.dir);
                let full = partial.completed(&display);
                let full = if matches!(quote_word(&full), Cow::Owned(_)) {
                    partial.expand(&full).unwrap_or(full)
                } else {
                    full
                };
                Candidate {
                    replacement: quote_word(&full).into_owned(),
                    display,
                }
            }
        }
    }
}

fn offer<'a>(needle: &str, pool: impl IntoIterator<Item = Item<'a>>) -> Vec<Candidate> {
    matching(needle, pool.into_iter().collect())
        .into_iter()
        .map(Item::candidate)
        .collect()
}

/// The `pool` items whose name extends `needle`, alphabetically; failing any,
/// the fuzzy matches, best first.  One decision over the whole pool: rustyline
/// splices the candidates' common prefix, so a mixed list would never extend
/// what was typed.
fn matching<T: AsRef<str>>(needle: &str, pool: Vec<T>) -> Vec<T> {
    let (mut hits, misses): (Vec<T>, Vec<T>) =
        pool.into_iter().partition(|i| extends(i.as_ref(), needle));
    if hits.is_empty() {
        return rank(needle, misses);
    }
    hits.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    hits
}

/// Smart case, as `rank` matches: a needle with no capital ignores case.
fn extends(name: &str, needle: &str) -> bool {
    if needle.chars().any(char::is_uppercase) {
        return name.starts_with(needle);
    }
    let mut name = name.chars().flat_map(char::to_lowercase);
    needle
        .chars()
        .flat_map(char::to_lowercase)
        .all(|c| name.next() == Some(c))
}

// ── The simple command at the cursor ─────────────────────────────────────────

/// The simple command around the cursor: `words[current]` is the word
/// being typed, starting at char `offset`.
#[derive(Debug, Default)]
pub(super) struct CommandAt {
    pub(super) words: Vec<String>,
    pub(super) current: usize,
    pub(super) offset: usize,
}

record!(typed CommandAt {
    words: "words",
    current: "current",
    offset: "offset",
});

/// True for token kinds that carry word-like content (command names,
/// arguments, variable references) as opposed to pure syntax (pipes,
/// braces, separators) that only delimit them.
fn is_word_token(tok: &Token) -> bool {
    matches!(
        tok,
        Token::Word(_)
            | Token::SingleQuoted(_)
            | Token::DoubleQuoted(_)
            | Token::Tag(_)
            | Token::Variable { .. }
            | Token::Expr(_)
    )
}

/// The text of a word-bearing token.  Single-quoted bodies come straight
/// from the token (already unescaped, hash-bumping and all); every other
/// kind is read back out of `text` via the token's own byte span, stripping
/// the surrounding quotes for double-quoted strings.
///
/// Escapes are asymmetric between the two quote styles: a single-quoted body
/// is the token's unescaped value, but a double-quoted body is returned raw
/// from the source span — its `\n`, `\"`, `$…` escapes verbatim, not
/// interpreted.  Plugins tokenizing the buffer see double-quoted text exactly
/// as typed; unescaping it is theirs to do if they need the runtime value.
fn word_text(text: &str, tok: &Token, span: ByteSpan) -> String {
    if let Token::SingleQuoted(s) = tok {
        return s.clone();
    }
    let start = span.start as usize;
    // A `$[` that `lex_open` closed ends past `text`.
    let end = (span.end as usize).min(text.len());
    match tok {
        Token::DoubleQuoted(_) => text[start + 1..end.saturating_sub(1).max(start + 1)].to_string(),
        _ => text[start..end].to_string(),
    }
}

/// The words of the simple command around char offset `cursor`, the index of
/// the word the cursor touches, and that word's char offset.  A cursor in a gap
/// gets a fresh `""` spliced in at its own offset, so `words[current]` is
/// always the word being typed (bash's `COMP_CWORD`, zsh's `CURRENT`).
pub(super) fn parse_at(text: &str, cursor: usize) -> Option<CommandAt> {
    let at = char_to_byte(text, cursor);
    let tokens = lex_open(text)?;
    let start = tokens
        .iter()
        .rposition(|l| separates(&l.token) && l.span.end as usize <= at)
        .map_or(0, |i| i + 1);
    let end = tokens[start..]
        .iter()
        .position(|l| separates(&l.token))
        .map_or(tokens.len(), |i| start + i);
    let (spans, mut words): (Vec<(usize, usize)>, Vec<String>) = tokens[start..end]
        .iter()
        .filter(|l| is_word_token(&l.token))
        .map(|Lexeme { token, span }| {
            (
                (span.start as usize, span.end as usize),
                word_text(text, token, *span),
            )
        })
        .unzip();
    if let Some(i) = spans.iter().position(|&(s, e)| s <= at && at <= e) {
        return Some(CommandAt {
            words,
            current: i,
            offset: byte_to_char(text, spans[i].0),
        });
    }
    let i = spans.iter().take_while(|&&(_, e)| e < at).count();
    words.insert(i, String::new());
    Some(CommandAt {
        words,
        current: i,
        offset: cursor,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;

    use ral_core::carrier::IdentityTransport;
    use ral_core::path::EnginePath;

    /// Backing storage for a hand-built [`Sources`], which borrows its cache.
    struct Fixture {
        cache: SourceCache,
        engine: IdentityTransport,
        /// The engine's logical cwd: a fresh directory holding only what a test
        /// plants in it, so path completion never lists the host's filesystem.
        cwd: tempfile::TempDir,
    }

    impl Fixture {
        fn view(&self) -> Sources<'_> {
            self.cache.sources(&self.engine)
        }
    }

    /// Backing store for the classification, ranking and path tests: every
    /// name goes in the shell half, the `PATH` scan is empty, and the engine
    /// sits in an empty directory of its own.
    fn sources(commands: &[&str], variables: &[&str]) -> Fixture {
        let cwd = tempfile::tempdir().unwrap();
        Fixture {
            cache: SourceCache {
                shell_commands: commands.iter().map(ToString::to_string).collect(),
                variables: variables.iter().map(ToString::to_string).collect(),
                key: PathKey::default(),
                scan: OnceCell::new(),
            },
            engine: engine_at("", cwd.path()),
            cwd,
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
        let attach = ral_core::protocol::Attach::root(
            ral_core::protocol::EngineSpec::new(
                crate::repl::TEST_TAG,
                EnginePath::Host(cwd.into()),
            ),
            ral_core::protocol::Root::new(EnginePath::Host(cwd.into()))
                .with_env(vec![("PATH".into(), path.into())]),
        );
        let runtime = ral_core::HostSurface::default()
            .runtime(ral_core::terminal::TerminalState::default())
            .expect("a test host resolves its floor");
        crate::repl::engine_at(&attach, runtime, |_| {})
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

    /// Command position is nothing before the atom, or a separator; anywhere
    /// else the atom is a path, completed from the cwd and not from commands.
    #[test]
    fn complete_offers_commands_after_separators() {
        let src = sources(&["git"], &[]);
        std::fs::write(src.cwd.path().join("gizmo"), "").unwrap();
        let offered = |line: &str| {
            let (at, cands) = complete(line, line.len(), &src.view());
            (at, cands.into_iter().map(|c| c.display).collect::<Vec<_>>())
        };
        for (line, start) in [("foo | gi", 6), ("if true { gi", 10), ("foo; gi", 5)] {
            assert_eq!(offered(line), (start, vec!["git".into()]), "{line:?}");
        }
        assert_eq!(
            offered("foo gi"),
            (4, vec!["gizmo".into()]),
            "an argument is a path"
        );
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

    fn displays(cands: &[Candidate]) -> Vec<&str> {
        cands.iter().map(|c| c.display.as_str()).collect()
    }

    #[test]
    fn complete_ignores_case_unless_the_needle_has_a_capital() {
        let src = sources(&["Foobar", "foobaz"], &[]);
        let lower = complete("foo", 3, &src.view()).1;
        assert_eq!(displays(&lower), ["Foobar", "foobaz"]);
        let capital = complete("Foo", 3, &src.view()).1;
        assert_eq!(displays(&capital), ["Foobar"]);
    }

    #[test]
    fn complete_falls_back_to_fuzzy_only_without_a_prefix_match() {
        let src = sources(&["ripgrep", "grep", "ls"], &[]);
        assert_eq!(displays(&complete("gr", 2, &src.view()).1), ["grep"]);
        assert_eq!(displays(&complete("rgp", 3, &src.view()).1), ["ripgrep"]);
    }

    #[test]
    fn a_prefix_match_anywhere_in_the_pool_suppresses_fuzzy() {
        let src = sources(&["sorcery"], &[]);
        assert_eq!(displays(&complete("src", 3, &src.view()).1), ["sorcery"]);
        std::fs::create_dir(src.cwd.path().join("src")).unwrap();
        let (_, cands) = complete("src", 3, &src.view());
        assert_eq!(displays(&cands), ["src/"]);
    }

    #[test]
    fn complete_offers_cwd_directories_at_command_position() {
        let src = sources(&["sort"], &[]);
        std::fs::create_dir(src.cwd.path().join("src")).unwrap();
        std::fs::write(src.cwd.path().join("srcfile"), "").unwrap();
        let (_, cands) = complete("s", 1, &src.view());
        assert_eq!(displays(&cands), ["sort", "src/"]);
        let (start, cands) = complete("sr", 2, &src.view());
        assert_eq!(start, 0);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].display, "src/");
        assert_eq!(cands[0].replacement, "src/");
    }

    // ── path completion ───────────────────────────────

    #[test]
    fn path_completion_expands_home_tilde_prefix() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete("ls ~/", 5, &sources(&[], &[]).view());
        assert_eq!(start, 3);
    }

    /// A lone `~` lists home, as the directory `~/`.
    #[test]
    fn path_completion_of_a_lone_tilde_lists_home() {
        if ral_core::host::home().is_none() {
            return;
        }
        let (start, _) = complete("ls ~", 4, &sources(&[], &[]).view());
        assert_eq!(start, 3);
    }
    // ── Engine cwd anchoring ────────────────────────────────────────────

    #[test]
    fn path_completion_lists_shell_cwd_for_empty_token() {
        let src = sources(&[], &[]);
        std::fs::write(src.cwd.path().join("alpha"), "").unwrap();
        std::fs::write(src.cwd.path().join("beta"), "").unwrap();

        let (start, cands) = complete("ls ", 3, &src.view());
        let mut names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        names.sort_unstable();
        assert_eq!((start, names), (3, vec!["alpha", "beta"]));
    }

    #[test]
    fn path_completion_lists_shell_cwd_subdir_for_relative_token() {
        let src = sources(&[], &[]);
        std::fs::create_dir(src.cwd.path().join("sub")).unwrap();
        std::fs::write(src.cwd.path().join("sub").join("gamma"), "").unwrap();

        let (start, cands) = complete("ls sub/", 7, &src.view());
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
        let src = sources(&[], &[]);
        std::fs::write(src.cwd.path().join("a b.txt"), "").unwrap();
        std::fs::write(src.cwd.path().join("plain.txt"), "").unwrap();
        std::fs::create_dir(src.cwd.path().join("sub")).unwrap();

        let (_, cands) = complete("ls ", 3, &src.view());
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
        let src = sources(&[], &[]);
        std::fs::create_dir(src.cwd.path().join("dir")).unwrap();
        std::fs::write(src.cwd.path().join("dir").join("a b"), "").unwrap();

        let (start, cands) = complete("ls dir/a", 8, &src.view());
        assert_eq!(start, 3);
        assert_eq!(cands[0].replacement, "'dir/a b'");
    }

    /// An open quote is one atom, spaces included; the replacement closes it.
    #[test]
    fn path_completion_continues_an_open_single_quote() {
        let src = sources(&[], &[]);
        std::fs::create_dir(src.cwd.path().join("my dir")).unwrap();

        let (start, cands) = complete("ls 'my di", 9, &src.view());
        assert_eq!(start, 3);
        assert_eq!(cands[0].replacement, "'my dir/'");
    }

    #[test]
    fn path_completion_leaves_absolute_dir_unanchored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("delta"), "").unwrap();

        // The fixture's cwd is wholly unrelated; the absolute prefix should win.
        let src = sources(&[], &[]);
        let line = format!("ls {}/", ral_core::path::shown(tmp.path()));
        let (_, cands) = complete(&line, line.len(), &src.view());
        let names: Vec<&str> = cands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(names, vec!["delta"]);
    }

    /// `parse_at` over `marked`, whose `▮` is the cursor, as
    /// `(words, current, offset)`.
    fn parse(marked: &str) -> Option<(Vec<String>, usize, usize)> {
        let at = marked.find('▮').expect("a cursor mark");
        parse_at(&marked.replace('▮', ""), marked[..at].chars().count())
            .map(|c| (c.words, c.current, c.offset))
    }

    fn w(words: &[&str]) -> Vec<String> {
        words.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn parse_at_reads_the_simple_command_at_the_cursor() {
        assert_eq!(parse("ssh me@h**▮"), Some((w(&["ssh", "me@h**"]), 1, 4)));
        assert_eq!(
            parse("ls -l | ssh h▮ ; cd x"),
            Some((w(&["ssh", "h"]), 1, 12))
        );
        assert_eq!(parse("ls▮| wc"), Some((w(&["ls"]), 0, 0)));
        assert_eq!(parse("cat a▮ b"), Some((w(&["cat", "a", "b"]), 1, 4)));
        assert_eq!(parse("echo é▮"), Some((w(&["echo", "é"]), 1, 5)));
    }

    /// A cursor between words is on a fresh empty word, at its own offset.
    #[test]
    fn parse_at_splices_an_empty_word_into_a_gap() {
        assert_eq!(parse("▮"), Some((w(&[""]), 0, 0)));
        assert_eq!(parse("ls | ssh ▮"), Some((w(&["ssh", ""]), 1, 9)));
        assert_eq!(parse("cp a ▮ b"), Some((w(&["cp", "a", "", "b"]), 2, 5)));
    }

    /// An open block or an open string is the editor's ordinary state, not a
    /// lex failure.
    #[test]
    fn parse_at_reads_inside_an_open_block_and_an_open_string() {
        assert_eq!(parse("try { cd ~/pr▮"), Some((w(&["cd", "~/pr"]), 1, 9)));
        assert_eq!(
            parse("within [dir: a] {\n  ssh ▮"),
            Some((w(&["ssh", ""]), 1, 24))
        );
        assert_eq!(parse("echo $[1 +▮"), Some((w(&["echo", "$[1 +"]), 1, 5)));
        assert_eq!(parse("echo 'open▮"), Some((w(&["echo", "open"]), 1, 5)));
    }

    #[test]
    fn the_fuzzy_tier_ranks_the_whole_pool_as_one() {
        let src = sources(&["carbonic"], &[]);
        std::fs::create_dir(src.cwd.path().join("src")).unwrap();
        let (_, cands) = complete("rc", 2, &src.view());
        assert_eq!(displays(&cands), ["src/", "carbonic"]);
    }

    #[test]
    fn extends_is_smart_case() {
        assert!(extends("Foobar", "foo"));
        assert!(!extends("foobar", "Foo"));
        assert!(extends("Foobar", "Foo"));
        assert!(extends("anything", ""));
        assert!(!extends("fo", "foo"));
    }
}
