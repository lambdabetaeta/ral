//! `PATH` search: locate a bare command name on disk.
//!
//! Sibling to the grant pipeline, but `$PATH` is only a colon-separated
//! list walked in turn, so the sigil/lex/canon stages do not apply.
//! Dispatch arrives via `runtime::command::identity` and completion via
//! [`commands_on_path`], both onto the same walk and the same
//! executable-bit rule.
//!
//! The walk costs one stat per `PATH` entry, and on Windows one per
//! `%PATHEXT%` suffix per entry, so [`locate`] memoises it for the extent of a
//! run; [`LOCATED`] argues what that costs in freshness.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The directory a `PATH` walk anchors its relative entries to.
///
/// A newtype rather than an `Option<&Path>` because "here" is a
/// *precedence* — the `within [dir: …]` override, else the `cd`-mutated cwd —
/// and a loose option lets each call site re-derive it.  Two walks that
/// answered the same question against two anchors is how a bare name in a
/// `cd`'d directory once earned "permission denied" from a walk that had
/// resolved nothing.  The constructors are therefore few and named for their
/// provenance: `Context::search_cwd` and [`Resolver::search_cwd`] carry the
/// precedence, [`SearchCwd::of`] serves a front end already holding it, and
/// [`SearchCwd::nowhere`] says outright that there is none.
///
/// [`Resolver::search_cwd`]: super::Resolver::search_cwd
#[derive(Clone, Copy)]
pub struct SearchCwd<'a>(Option<&'a Path>);

impl<'a> SearchCwd<'a> {
    /// For a caller already holding the shell's effective cwd — `Shell::cwd`,
    /// which adds the process-cwd fallback, and the REPL's completion scan.
    /// Core runtime code goes through `Context::search_cwd` instead, so the
    /// override-vs-`cd` precedence cannot be re-chosen a third way.
    #[must_use]
    pub fn of(cwd: &'a Path) -> Self {
        Self(Some(cwd))
    }

    /// No anchor: a shell-less caller, or a test that names absolute entries.
    /// What it costs is that a relative `PATH` entry stats against the
    /// *process* cwd, which no `cd` of ral's ever moves.
    #[must_use]
    pub fn nowhere() -> Self {
        Self(None)
    }
}

/// First executable named `name` on the colon-separated `path`, with
/// relative `PATH` entries anchored to `cwd`; `None` when `name` bears a
/// separator, which is a path, not `PATH`'s business.
pub fn resolve_in_path(name: &str, path: &str, cwd: SearchCwd<'_>) -> Option<String> {
    if name_has_separator(name) {
        return None;
    }
    locate(name, Some(path), cwd).map(|p| p.to_string_lossy().into_owned())
}

/// Resolve a command head to its executable on disk: a separator-bearing
/// name is a path anchored against `cwd`, a bare name is walked against
/// `path_value`.
///
/// `Some` only for a regular file carrying the executable bit — the check
/// the OS would apply at spawn.  A bare name's answer is memoised for the
/// current run ([`LOCATED`]).
pub fn locate(name: &str, path_value: Option<&str>, cwd: SearchCwd<'_>) -> Option<PathBuf> {
    locate_at(name, path_value, cwd, Instant::now())
}

/// [`locate`] with the clock passed in, so the negative-answer TTL is
/// testable without sleeping through it.
fn locate_at(
    name: &str,
    path_value: Option<&str>,
    cwd: SearchCwd<'_>,
    now: Instant,
) -> Option<PathBuf> {
    // Answered before the memo is touched: one join and one stat is no
    // amplification to amortise, and a caller naming a file usually means
    // "is it there *now*" — a binary this very run built.
    if name_has_separator(name) {
        let candidate = anchor_to_cwd(PathBuf::from(name), cwd);
        return is_executable_file(&candidate).then_some(candidate);
    }
    let path_value = path_value?;
    #[cfg(windows)]
    let suffixes = windows_pathext_suffixes();
    let key = LocateKey {
        name: name.to_owned(),
        path_value: path_value.to_owned(),
        cwd: cwd.0.map(Path::to_path_buf),
        #[cfg(windows)]
        pathext: suffixes.join(";"),
    };
    if let Recalled::Remembered(answer) = recall(&key, now) {
        return answer;
    }
    let found = 'walk: {
        for dir in path_dirs(path_value, cwd) {
            let candidate = dir.join(name);
            #[cfg(windows)]
            for c in windows_command_candidates(&candidate, &suffixes) {
                if is_executable_file(&c) {
                    break 'walk Some(c);
                }
            }
            #[cfg(not(windows))]
            if is_executable_file(&candidate) {
                break 'walk Some(candidate);
            }
        }
        None
    };
    remember(key, found.clone(), now);
    found
}

/// How long a *negative* answer is trusted, inside the run that took it:
/// short enough that `cargo install foo; foo` in one dispatch works.
const NEGATIVE_TTL: Duration = Duration::from_secs(1);

/// Bumped by [`forget_located_commands`]; a thread's memo is stale the moment
/// its stamp differs.  Process-global while the map is per-thread, so a bump
/// made anywhere is honoured everywhere — conservatively, since a thread that
/// remembered nothing loses nothing by clearing.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Located {
    hit: Option<PathBuf>,
    probed: Instant,
}

/// Everything a bare-name answer depends on.
///
/// `path_value` is the whole effective `PATH` string, which is what makes a
/// `within [shell: PATH=…]` override correct by construction: a block with its
/// own search list asks a different question and gets its own entry, with
/// nothing to invalidate on the way in or out.  `cwd` is here because
/// [`path_dirs`] anchors relative entries against it, and on Windows
/// `%PATHEXT%` because the suffix list is read from the process environment
/// rather than from `Context::env_overrides`.  That last clause names no item:
/// the reader of it is `cfg(windows)`-only, and a link from this unguarded
/// doc would fail the deny on broken links everywhere else.
#[derive(PartialEq, Eq, Hash)]
struct LocateKey {
    name: String,
    path_value: String,
    cwd: Option<PathBuf>,
    #[cfg(windows)]
    pathext: String,
}

struct LocateCache {
    generation: u64,
    entries: HashMap<LocateKey, Located>,
}

thread_local! {
    /// A `PATH` walk paid once per run rather than once per name.
    ///
    /// Thread-local and lock-free, because a run's evaluation is
    /// single-threaded on the calling thread.  It hangs off no `Shell` and no
    /// `Context` on purpose: two of the hottest callers,
    /// `runtime::command::identity`'s `walk_path` and `policy_names`, hold
    /// only a `&Context`.
    ///
    /// A newly installed or newly deleted executable is therefore invisible
    /// for at most the remainder of the current top-level run — one submitted
    /// line in the REPL, one script in batch, one dispatch in the engine — and
    /// nothing survives a run boundary, which is fresher than bash's `hash`
    /// table.  Within a run a `None` older than [`NEGATIVE_TTL`] is re-probed
    /// while positives ride the generation alone, because a stale miss is a
    /// wrong answer where a stale hit is a spawn that fails with the OS's own
    /// ENOENT, and whose miss sends [`search`] on to its uncached presence
    /// half to choose between 126 and 127.  Misses are also
    /// the case worth caching: `evaluator::pattern`'s shadow check walks the
    /// whole list to the end for every binding name.
    static LOCATED: RefCell<LocateCache> = RefCell::new(LocateCache {
        generation: GENERATION.load(Ordering::Relaxed),
        entries: HashMap::new(),
    });
}

/// What the memo has to say about a name.
///
/// Three named states rather than a nested `Option`, because "nothing
/// remembered" and "a miss worth remembering" send the caller opposite ways —
/// one walks, one answers — and reading both off the same `None` is exactly
/// how that distinction goes missing.
enum Recalled {
    /// No live memory: the caller walks and then [`remember`]s.
    Cold,
    /// The walk's own answer, `None` where it found nothing.
    Remembered(Option<PathBuf>),
}

/// What is remembered for `key`, as of `now`.
fn recall(key: &LocateKey, now: Instant) -> Recalled {
    LOCATED.with_borrow_mut(|cache| {
        let live = GENERATION.load(Ordering::Relaxed);
        if cache.generation != live {
            cache.entries.clear();
            cache.generation = live;
        }
        let Some(entry) = cache.entries.get(key) else {
            return Recalled::Cold;
        };
        // An aged-out negative reads as no memory at all, so the walk below
        // replaces it rather than anything having to sweep it.
        if entry.hit.is_none() && now.duration_since(entry.probed) >= NEGATIVE_TTL {
            return Recalled::Cold;
        }
        Recalled::Remembered(entry.hit.clone())
    })
}

/// Remember `hit` for `key`, stamped `now`.
fn remember(key: LocateKey, hit: Option<PathBuf>, now: Instant) {
    LOCATED.with_borrow_mut(|cache| {
        cache.entries.insert(key, Located { hit, probed: now });
    });
}

/// Forget every memoised `PATH` answer, on every thread: the next [`locate`]
/// walks the disk again.
///
/// The top-level run door calls this, so a caller needs it only to admit a
/// *mid-run* change to the filesystem — planting a binary and resolving it in
/// the same dispatch.  What staleness that leaves is argued at [`LOCATED`].
pub fn forget_located_commands() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

fn name_has_separator(name: &str) -> bool {
    name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') || name.contains('\\')
}

/// Anchor a relative entry to `cwd`, folding `.` and `..` out of the join.
///
/// The fold is not cosmetic: a resolved path travels on to the OS sandbox
/// profile via [`capability::sandbox`](crate::capability::sandbox), which
/// matches literally, and the `/work/./bin/git` an unfolded `./bin` yields
/// is covered by no `/work/bin/` the profile names.
fn anchor_to_cwd(p: PathBuf, cwd: SearchCwd<'_>) -> PathBuf {
    let joined = match cwd.0 {
        Some(c) if !p.is_absolute() => c.join(p),
        _ => p,
    };
    super::lex::fold_dots(&joined)
}

/// The one directory list behind [`locate`], [`commands_on_path`], and
/// [`search`]; relative entries (`./bin`) anchor to `cwd`.
///
/// An **empty element is dropped, on every platform**.  POSIX reads one as the
/// cwd — the forty-year-old implicit-`.`-on-`PATH` foot-gun — and on Windows a
/// trailing `;` is ubiquitous noise, so honouring it would put every file of
/// every directory the user `cd`s into on the search list; off Unix, where
/// [`is_executable_file`] calls any file executable, that is every file
/// outright.  A user who wants the cwd searched writes `.`, which
/// [`anchor_to_cwd`] honours deliberately.  The filter runs on the element as
/// written, before anchoring turns `""` into the cwd itself.
fn path_dirs(path_value: &str, cwd: SearchCwd<'_>) -> Vec<PathBuf> {
    std::env::split_paths(&std::ffi::OsString::from(path_value))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| anchor_to_cwd(dir, cwd))
        .collect()
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:which-stat] `which`/PATH probe: stats a candidate to read its executable bit; an executable-probe predicate, not turn-time model data I/O, raises no surface card."
)]
fn is_executable_file(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Names of the executables reachable through `path_value`, in `PATH`
/// order; unreadable entries are skipped.
///
/// Unsorted, and a name repeats once per directory holding it — completion
/// sorts and dedupes its own.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:which-readdir] `which`/completion probe: enumerates each PATH directory to list executable names; an executable-probe scan, not turn-time model data I/O, raises no surface card."
)]
pub fn commands_on_path(path_value: &str, cwd: SearchCwd<'_>) -> Vec<String> {
    let mut out = Vec::new();
    for dir in path_dirs(path_value, cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                out.push(name);
            }
        }
    }
    out
}

/// What one walk of `PATH` found, in the three states `runtime::command::vet`
/// must tell apart.
///
/// Three, because the 126/127 split is a property *of the walk*: a verdict
/// computed by a second, independent traversal can disagree with the first
/// about the anchor, the search list, or the moment — and a disagreement in
/// that shape reads exactly like "on `PATH` but lacking `+x`".  Carrying the
/// verdict out of the traversal that produced the resolution leaves nothing
/// for the two to disagree about.
#[derive(Clone, Debug)]
pub(crate) enum PathSearch {
    /// A regular file carrying the executable bit: what the OS would spawn.
    Executable(PathBuf),
    /// A file of that name is there, but the walk would not spawn it — 126.
    FoundNotExecutable(PathBuf),
    /// No file of that name anywhere on the list — 127.
    Missing,
}

/// Walk `PATH` for `name` once and report all three outcomes.
///
/// The executable half is [`locate`], memo and TTL unchanged.  The
/// presence half runs only when that misses — the error path, and bundled
/// names whose `PATH` holds no host twin — and stays uncached, for the reason
/// [`LOCATED`] gives: a stale *hit* costs a spawn that fails with the OS's own
/// ENOENT, but a stale answer here is the difference between two exit codes a
/// user reads as different diagnoses.  It walks the same [`path_dirs`] list as
/// the executable half, so no empty entry, no anchor and no suffix rule can
/// differ between them.
///
/// A separator-bearing name is [`PathSearch::Missing`] outright, as in
/// [`resolve_in_path`]: it is a path, not `PATH`'s business, and the kernel
/// gives the better diagnosis at spawn.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:which-stat] `which`/PATH probe: stats a candidate to tell an unexecutable file from an absent one; an executable-probe predicate, not turn-time model data I/O, raises no surface card."
)]
pub(crate) fn search(name: &str, path_value: Option<&str>, cwd: SearchCwd<'_>) -> PathSearch {
    if name_has_separator(name) {
        return PathSearch::Missing;
    }
    if let Some(hit) = locate(name, path_value, cwd) {
        return PathSearch::Executable(hit);
    }
    let Some(path_value) = path_value else {
        return PathSearch::Missing;
    };
    path_dirs(path_value, cwd)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map_or(PathSearch::Missing, PathSearch::FoundNotExecutable)
}

/// The resolver's fallback when `%PATHEXT%` is unset; `capability::exec`
/// keeps the stripped twin, and the agreement is pinned by its tests.
#[cfg(windows)]
pub(crate) const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The Windows resolver's `%PATHEXT%` suffixes, leading dots stripped, with
/// the resolver's own default when the variable is unset.
///
/// Read once per [`locate`] call rather than once per `PATH` directory: the
/// list is the same for every directory in a walk, so re-reading and
/// re-splitting the environment at each one bought an env lookup, a split and
/// a fresh allocation per entry and no freshness anyone could observe.  The
/// list is also an input to [`LocateKey`], which needs it computed here anyway.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "PATHEXT is the Windows resolver's suffix list, not an XDG basedir — a PATH-probe env read, allowed at the call site like the other which/PATH probes here"
)]
fn windows_pathext_suffixes() -> Vec<String> {
    use std::ffi::OsStr;
    let pathext =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsStr::new(DEFAULT_PATHEXT).to_os_string());
    pathext
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|ext| ext.trim_start_matches('.').to_owned())
        .collect()
}

/// Mirror the Windows resolver's `%PATHEXT%` fallback, so
/// `locate("python")` finds `python.exe`.  `capability::exec` keeps its
/// own copy of the default list to strip suffixes off grant keys.
///
/// The resolver *appends* each suffix; it never substitutes one.  Building the
/// candidates with `Path::with_extension` — which replaces — resolved a bare
/// `build.ps1` to whatever `build.exe` sat first on `PATH`, a different program
/// than the user named.  Appending to the literal as written (`build.ps1.EXE`)
/// is what `cmd.exe` itself does, and is pinned by
/// `windows_tests::pathext_appends_never_replaces`.
#[cfg(windows)]
fn windows_command_candidates(base: &Path, suffixes: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if base.extension().is_some() {
        out.push(base.to_path_buf());
    }
    out.extend(
        suffixes
            .iter()
            .map(|ext| with_appended_suffix(base, ext.as_str())),
    );
    out
}

/// `base` with `.ext` appended: `build.ps1` + `EXE` is `build.ps1.EXE`.
///
/// Spelled by concatenation on the `OsStr`, so there is no `with_extension`
/// left in the candidate builder for a later reading to flip back.
#[cfg(windows)]
fn with_appended_suffix(base: &Path, ext: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// The memo's tests, on every platform — separate from the walk's `tests`
/// below only because those are written against Unix permission bits.
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod memo_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// [`GENERATION`] is process-global, so a concurrent sibling's
    /// [`forget_located_commands`] would clear the memo a test is asserting
    /// about.  Poison-tolerant, so a failure does not wedge the rest.
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Take the lock and start from a known generation.
    pub(super) fn cache_guard() -> MutexGuard<'static, ()> {
        let guard = CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        forget_located_commands();
        guard
    }

    /// Whatever the walk's executable test demands: the `+x` bit on Unix, and
    /// nothing off it, where [`is_executable_file`] accepts any file.
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

    /// An executable a *bare-name* walk will find in `dir`.  Off Unix a bare
    /// name only resolves through `%PATHEXT%`, so the file needs a suffix from
    /// that list; on Unix the stem is the whole name.
    fn plant(dir: &Path, stem: &str) -> PathBuf {
        let planted = if cfg!(windows) {
            dir.join(format!("{stem}.bat"))
        } else {
            dir.join(stem)
        };
        make_executable(&planted);
        planted
    }

    fn as_path_value(dir: &Path) -> String {
        dir.to_str().unwrap().to_owned()
    }

    #[test]
    fn a_memoised_lookup_returns_the_same_answer() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let planted = plant(tmp.path(), "runme");
        let path_value = as_path_value(tmp.path());

        let first = locate("runme", Some(&path_value), SearchCwd::nowhere())
            .expect("planted binary must resolve");
        std::fs::remove_file(&planted).unwrap();
        // The file is gone, so a second walk could only answer `None`.
        assert_eq!(
            locate("runme", Some(&path_value), SearchCwd::nowhere()),
            Some(first)
        );
    }

    #[test]
    fn forgetting_re_probes_a_deleted_executable() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let planted = plant(tmp.path(), "runme");
        let path_value = as_path_value(tmp.path());

        assert!(locate("runme", Some(&path_value), SearchCwd::nowhere()).is_some());
        std::fs::remove_file(&planted).unwrap();
        forget_located_commands();
        assert_eq!(
            locate("runme", Some(&path_value), SearchCwd::nowhere()),
            None
        );
    }

    #[test]
    fn forgetting_re_probes_a_newly_installed_executable() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let path_value = as_path_value(tmp.path());

        // A remembered miss must not outlive the run door's forget, or
        // `cargo install foo` then `foo` still says "not found".
        assert_eq!(
            locate("runme", Some(&path_value), SearchCwd::nowhere()),
            None
        );
        plant(tmp.path(), "runme");
        forget_located_commands();
        assert!(locate("runme", Some(&path_value), SearchCwd::nowhere()).is_some());
    }

    #[test]
    fn a_negative_answer_re_probes_when_its_ttl_expires() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let path_value = as_path_value(tmp.path());
        let probed = Instant::now();

        assert_eq!(
            locate_at("runme", Some(&path_value), SearchCwd::nowhere(), probed),
            None
        );
        plant(tmp.path(), "runme");
        assert_eq!(
            locate_at("runme", Some(&path_value), SearchCwd::nowhere(), probed),
            None,
            "inside the TTL the remembered miss stands"
        );
        assert!(
            locate_at(
                "runme",
                Some(&path_value),
                SearchCwd::nowhere(),
                probed + NEGATIVE_TTL
            )
            .is_some(),
            "past it the walk runs again"
        );
    }

    #[test]
    fn a_different_path_override_does_not_read_another_paths_answer() {
        let _guard = cache_guard();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        plant(a.path(), "runme");
        plant(b.path(), "runme");

        let from_a = locate(
            "runme",
            Some(&as_path_value(a.path())),
            SearchCwd::nowhere(),
        )
        .unwrap();
        let from_b = locate(
            "runme",
            Some(&as_path_value(b.path())),
            SearchCwd::nowhere(),
        )
        .unwrap();
        assert!(from_a.starts_with(a.path()), "got {from_a:?}");
        // A `within [shell: PATH=…]` block cannot be handed the enclosing
        // scope's answer.
        assert!(from_b.starts_with(b.path()), "got {from_b:?}");
    }

    #[test]
    fn a_changed_cwd_re_anchors_a_relative_entry() {
        let _guard = cache_guard();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let bin = a.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        plant(&bin, "runme");

        assert!(locate("runme", Some("./bin"), SearchCwd::of(a.path())).is_some());
        // Same name and same `PATH`; only the anchor differs.
        assert_eq!(
            locate("runme", Some("./bin"), SearchCwd::of(b.path())),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_changed_pathext_re_probes() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("runme.bat"));
        let path_value = as_path_value(tmp.path());

        // No `forget` between the two: `%PATHEXT%` is in the key itself.
        let without = crate::test_env::with_var("PATHEXT", Some(".EXE"), || {
            locate("runme", Some(&path_value), SearchCwd::nowhere())
        });
        let with = crate::test_env::with_var("PATHEXT", Some(".BAT"), || {
            locate("runme", Some(&path_value), SearchCwd::nowhere())
        });
        assert_eq!(without, None);
        assert!(with.is_some(), "got {with:?}");
    }

    #[test]
    fn a_separator_bearing_name_is_never_memoised() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let planted = tmp.path().join("runme");
        make_executable(&planted);

        assert!(locate("./runme", None, SearchCwd::of(tmp.path())).is_some());
        std::fs::remove_file(&planted).unwrap();
        // No `forget`, and still a fresh answer.
        assert_eq!(locate("./runme", None, SearchCwd::of(tmp.path())), None);
    }

    /// `PATH` with a trailing separator, so the split yields an empty element.
    fn with_trailing_separator(dir: &Path) -> String {
        let sep = if cfg!(windows) { ';' } else { ':' };
        format!("{}{sep}", as_path_value(dir))
    }

    #[test]
    fn an_empty_path_entry_never_means_the_cwd() {
        let _guard = cache_guard();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        plant(here.path(), "runme");
        let path_value = with_trailing_separator(elsewhere.path());

        // The trailing separator is the user's `PATH` ending in `;` — noise,
        // not a request to search wherever they happen to stand.
        assert_eq!(
            locate("runme", Some(&path_value), SearchCwd::of(here.path())),
            None,
        );
    }

    #[test]
    fn search_misses_the_cwd_through_an_empty_entry() {
        let _guard = cache_guard();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        plant(here.path(), "runme");
        let path_value = with_trailing_separator(elsewhere.path());

        // The presence half walks the same list, so it cannot see a file the
        // executable half was never offered — which is what a spurious 126 is.
        assert!(matches!(
            search("runme", Some(&path_value), SearchCwd::of(here.path())),
            PathSearch::Missing,
        ));
    }

    #[test]
    fn commands_on_path_ignores_empty_entries() {
        let _guard = cache_guard();
        let here = tempfile::tempdir().unwrap();
        plant(here.path(), "runme");

        let names = commands_on_path("", SearchCwd::of(here.path()));
        assert!(names.is_empty(), "got {names:?}");
    }
}

/// The Windows resolver's own rules: `%PATHEXT%` suffixes append to the name
/// the user wrote, and never stand in for one they did.
#[cfg(test)]
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod windows_tests {
    use super::*;

    #[test]
    fn pathext_appends_never_replaces() {
        let suffixes = ["EXE".to_owned(), "BAT".to_owned()];
        let got = windows_command_candidates(Path::new("build.ps1"), &suffixes);
        assert_eq!(
            got,
            vec![
                PathBuf::from("build.ps1"),
                PathBuf::from("build.ps1.EXE"),
                PathBuf::from("build.ps1.BAT"),
            ],
        );
        assert!(
            !got.contains(&PathBuf::from("build.exe")),
            "a suffixed name must never become a sibling extension: {got:?}",
        );
    }

    #[test]
    fn a_suffixed_name_does_not_resolve_to_a_sibling_extension() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("build.exe"), b"MZ").unwrap();
        let path_value = tmp.path().to_str().unwrap().to_owned();

        crate::test_env::with_var("PATHEXT", Some(".EXE"), || {
            assert_eq!(
                locate("build.ps1", Some(&path_value), SearchCwd::nowhere()),
                None,
                "`build.ps1` names a PowerShell script, not whatever build.exe is first on PATH",
            );
            forget_located_commands();
            assert!(locate("build", Some(&path_value), SearchCwd::nowhere()).is_some());
        });
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn touch(p: &Path, mode: u32) {
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn commands_on_path_finds_executables() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("runme"), 0o755);
        let names = commands_on_path(tmp.path().to_str().unwrap(), SearchCwd::nowhere());
        assert!(names.contains(&"runme".to_string()), "got {names:?}");
    }

    #[test]
    fn commands_on_path_skips_non_executable_files() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("noexec"), 0o644);
        let names = commands_on_path(tmp.path().to_str().unwrap(), SearchCwd::nowhere());
        assert!(!names.contains(&"noexec".to_string()), "got {names:?}");
    }

    #[test]
    fn commands_on_path_anchors_relative_entries_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        touch(&bin.join("runme"), 0o755);
        // Against the supplied cwd, not the process cwd: otherwise the
        // prompt stops reflecting the shell's notion of "here".
        let names = commands_on_path("./bin", SearchCwd::of(tmp.path()));
        assert!(names.contains(&"runme".to_string()), "got {names:?}");
    }

    #[test]
    fn locate_folds_the_dots_out_of_an_anchored_path() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("runme"), 0o755);
        // Folded, not merely joined: the answer is what the OS sandbox
        // profile matches literally, and `/tmp/x/./runme` is not `/tmp/x/runme`.
        let hit = locate("./runme", None, SearchCwd::of(tmp.path())).unwrap();
        assert_eq!(hit, tmp.path().join("runme"));
    }

    #[test]
    fn resolve_in_path_folds_the_dots_out_of_a_relative_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        touch(&bin.join("runme"), 0o755);
        let hit = resolve_in_path("runme", "./bin", SearchCwd::of(tmp.path())).unwrap();
        assert_eq!(hit, bin.join("runme").to_str().unwrap());
    }

    /// The 126 case, and the only platform that has one: the presence half
    /// answers where the executable half declined, off the same walk.
    #[test]
    fn search_reports_found_not_executable_on_the_same_walk() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("noexec"), 0o644);
        let path_value = tmp.path().to_str().unwrap();

        assert_eq!(
            locate("noexec", Some(path_value), SearchCwd::nowhere()),
            None
        );
        match search("noexec", Some(path_value), SearchCwd::nowhere()) {
            PathSearch::FoundNotExecutable(p) => assert_eq!(p, tmp.path().join("noexec")),
            other => panic!("expected FoundNotExecutable, got {other:?}"),
        }
    }

    #[test]
    fn commands_on_path_skips_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("does-not-exist");
        let names = commands_on_path(absent.to_str().unwrap(), SearchCwd::nowhere());
        assert!(names.is_empty(), "got {names:?}");
    }
}
