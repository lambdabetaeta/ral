//! `PATH` search: locate a bare command name on disk.
//!
//! Sibling to the grant pipeline, but `$PATH` is only a list of
//! directories walked in turn, so the sigil/lex/canon stages do not apply.
//! Dispatch arrives via `runtime::command::head`.
//!
//! The walk costs one stat per `PATH` entry, and on Windows one per
//! `%PATHEXT%` suffix per entry, so [`locate`] memoises it for the extent of a
//! run; [`LOCATED`] argues what that costs in freshness.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::site::Silent;
use super::{CwdPath, PathRules};
use crate::site::site;
use crate::types::EnvVars;

/// Resolve a command head to its executable on disk: a separator-bearing
/// name is a path anchored against `cwd`, a bare name is walked against
/// `path_value`.
///
/// `Some` only for a regular file carrying the executable bit — the check
/// the OS would apply at spawn.  A bare name's answer is memoised for the
/// current run ([`LOCATED`]).
pub fn locate(name: &str, path_value: Option<&str>, cwd: Option<&CwdPath>) -> Option<PathBuf> {
    locate_at(name, path_value, cwd, Instant::now())
}

/// [`locate`] with the clock passed in, so the negative-answer TTL is
/// testable without sleeping through it.
fn locate_at(
    name: &str,
    path_value: Option<&str>,
    cwd: Option<&CwdPath>,
    now: Instant,
) -> Option<PathBuf> {
    // Answered before the memo is touched: one join and one stat is no
    // amplification to amortise, and a caller naming a file usually means
    // "is it there *now*": a binary this very run built.
    if names_a_path(name) {
        let candidate = anchor_to_cwd(PathBuf::from(name), cwd)?;
        return is_executable_file(&candidate).then_some(candidate);
    }
    let path_value = path_value?;
    #[cfg(windows)]
    let suffixes = windows_pathext_suffixes();
    let key = LocateKey {
        name: name.to_owned(),
        path_value: path_value.to_owned(),
        cwd: cwd.map(|c| c.as_path().to_path_buf()),
        #[cfg(windows)]
        pathext: suffixes.join(";"),
    };
    if let Recalled::Remembered(answer) = recall(&key, now) {
        return answer;
    }
    #[cfg(windows)]
    let found = path_dirs(path_value, cwd).into_iter().find_map(|dir| {
        windows_command_candidates(&dir.join(name), &suffixes)
            .into_iter()
            .find(|c| is_executable_file(c))
    });
    #[cfg(not(windows))]
    let found = locate_in(name, path_dirs(path_value, cwd));
    remember(key, found.clone(), now);
    found
}

/// The first executable file named `name` in `dirs`, in order and
/// unmemoised: the walk [`locate`] makes off Windows, for a caller already
/// holding the directories rather than a `PATH` string.
#[cfg(not(windows))]
pub(crate) fn locate_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
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
    /// `Context` on purpose: the hottest callers, `runtime::command::head`'s
    /// resolution and `capability::exec`'s compile, hold no `Shell`.
    ///
    /// A newly installed or newly deleted executable is therefore invisible
    /// for at most the remainder of the current top-level run — one submitted
    /// line in the REPL, one script in batch, one dispatch in the engine — and
    /// nothing survives a run boundary, which is fresher than bash's `hash`
    /// table.  Within a run a `None` older than [`NEGATIVE_TTL`] is re-probed
    /// while positives ride the generation alone, because a stale miss is a
    /// wrong answer where a stale hit is a deleted file, which `Program::file`
    /// reports as `Missing::NotFound` (127) without resuming the walk.
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
/// The top-level run calls this, so a caller needs it only to admit a
/// *mid-run* change to the filesystem — planting a binary and resolving it in
/// the same dispatch.  What staleness that leaves is argued at [`LOCATED`].
pub fn forget_located_commands() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Whether a command name is a path rather than a name for `PATH` to find:
/// it bears a separator under the host's rules.
pub(crate) fn names_a_path(name: &str) -> bool {
    names_a_path_for(name, PathRules::HOST)
}

/// [`names_a_path`] under given rules: so `a\b` is a path under Windows
/// and a bare name, searched on `PATH`, under Posix.
pub(crate) fn names_a_path_for(name: &str, rules: PathRules) -> bool {
    name.contains(|c| rules.is_separator(c))
}

/// Anchor a relative entry to `cwd`, folding `.` and `..` out of the join.
///
/// The fold is not cosmetic: a resolved path travels on to the OS sandbox
/// profile via [`SandboxProjection::of`](crate::sandbox::SandboxProjection::of), which
/// matches literally, and the `/work/./bin/git` an unfolded `./bin` yields
/// is covered by no `/work/bin/` the profile names.  With no cwd a relative
/// entry names nothing: there is no "here" to read it against.
fn anchor_to_cwd(p: PathBuf, cwd: Option<&CwdPath>) -> Option<PathBuf> {
    let joined = match cwd {
        _ if super::lex::is_absolute(&p) => p,
        Some(c) => c.as_path().join(p),
        None => return None,
    };
    Some(super::lex::fold_dots(&joined))
}

/// The one directory list behind [`locate`] and [`search`]; relative
/// entries (`./bin`) anchor to `cwd`, and with no cwd are dropped, as a
/// grant's `path` selector and the bwrap search read `PATH`.
pub fn path_dirs(path_value: impl AsRef<OsStr>, cwd: Option<&CwdPath>) -> Vec<PathBuf> {
    path_entries(path_value.as_ref())
        .filter_map(|dir| anchor_to_cwd(dir, cwd))
        .collect()
}

/// The `RAL_PATH` directories in declaration order, as written; empty when
/// unset.  A PATH-style list: `env` before the host, the overlay-on-process
/// rule `within [env: …]` obeys.
pub fn ral_path_entries(env: &EnvVars) -> Vec<PathBuf> {
    let raw = env.get_or_host("RAL_PATH").unwrap_or_default();
    path_entries(OsStr::new(&raw)).collect()
}

/// The entries of a `PATH`-style list, in order and as written, split on the
/// host's separator (`:`, or `;` on Windows): the one place such a list is
/// split.
///
/// An **empty element is dropped, on every platform**.  POSIX reads one as the
/// cwd — the forty-year-old implicit-`.`-on-`PATH` foot-gun — and on Windows a
/// trailing `;` is ubiquitous noise, so honouring it would put every file of
/// every directory the user `cd`s into on the search list; off Unix, where
/// [`is_executable_file`] calls any file executable, that is every file
/// outright.  A user who wants the cwd searched writes `.`, which
/// [`anchor_to_cwd`] honours deliberately.
pub fn path_entries(path_value: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path_value).filter(|dir| !dir.as_os_str().is_empty())
}

pub(crate) fn is_executable_file(p: &Path) -> bool {
    if !site!(Silent::WhichStat, p => is_file()) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        site!(Silent::WhichStat, std::fs::metadata(p))
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
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
/// [`LOCATED`] gives: a stale *hit* is caught at `Program::file`, which finds
/// the file gone and answers 127, but a stale answer here is the difference
/// between two exit codes a user reads as different diagnoses.  It walks the
/// same [`path_dirs`] list as the executable half, so no empty entry, no
/// anchor and no suffix rule can differ between them.
///
/// A separator-bearing name is [`PathSearch::Missing`] outright: it is a
/// path, not `PATH`'s business, and the kernel gives the better diagnosis at
/// spawn.
pub(crate) fn search(name: &str, path_value: Option<&str>, cwd: Option<&CwdPath>) -> PathSearch {
    if names_a_path(name) {
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
        .find(|candidate| site!(Silent::WhichStatAbsent, candidate => is_file()))
        .map_or(PathSearch::Missing, PathSearch::FoundNotExecutable)
}

/// The resolver's fallback when `%PATHEXT%` is unset;
/// [`WINDOWS_EXEC_EXTENSIONS`] is its stripped twin, the agreement pinned
/// below.
#[cfg(windows)]
pub(crate) const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The extensions Windows runs a bare name through.  `.bat` and `.cmd` belong
/// here even though `process::launch` refuses to spawn them: that is a later
/// refusal of the image, not of the name.
pub(crate) const WINDOWS_EXEC_EXTENSIONS: &[&str] = &["com", "exe", "bat", "cmd"];

/// The [`WINDOWS_EXEC_EXTENSIONS`] Windows runs through `cmd.exe`, which
/// re-parses the command line by its own escaping rules.
#[cfg(any(windows, test))]
const WINDOWS_BATCH_EXTENSIONS: &[&str] = &["bat", "cmd"];

/// Whether `program` names a batch image as Windows will run it: once
/// [Win32 has trimmed](super::device::win32_trimmed) its trailing dots and
/// blanks, it ends in `.` and one of [`WINDOWS_BATCH_EXTENSIONS`], in any
/// case, so `run.bat.` and `run.bat ` are batch images too, and `.bat`
/// alone is one, as `std`'s own check reads it.  Trimmed under a verbatim
/// head as well, where Win32 would not: a refusal that over-reads fails
/// closed.
///
/// Read on the name's encoded bytes, so nothing is decoded and an ill-formed
/// name is judged by the suffix it has.
#[cfg(any(windows, test))]
pub(crate) fn is_batch_image(program: &OsStr) -> bool {
    let trimmed = super::device::win32_trimmed(program.as_encoded_bytes());
    trimmed.iter().rposition(|&b| b == b'.').is_some_and(|dot| {
        let ext = &trimmed[dot + 1..];
        WINDOWS_BATCH_EXTENSIONS
            .iter()
            .any(|batch| ext.eq_ignore_ascii_case(batch.as_bytes()))
    })
}

/// A command name as a deny holds it: the host's name for it, under every
/// spelling some filesystem takes for that.
pub(crate) fn command_name_key(name: &str) -> String {
    super::identity::collision_fold(&name_key_on(name, PathRules::HOST))
}

/// Off Windows the name itself; on Windows ASCII lower-case, a trailing
/// executable extension stripped.
fn name_key_on(name: &str, rules: PathRules) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if rules == PathRules::Posix {
        return Cow::Borrowed(name);
    }
    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((stem, ext)) if WINDOWS_EXEC_EXTENSIONS.contains(&ext) => Cow::Owned(stem.to_string()),
        _ => Cow::Owned(lower),
    }
}

/// The Windows resolver's `%PATHEXT%` suffixes, leading dots stripped, with
/// the resolver's own default when the variable is unset.  An entry that is
/// not Unicode is dropped, since a suffix is appended as text.
///
/// Read once per [`locate`] call rather than once per `PATH` directory: the
/// list is the same for every directory in a walk, so re-reading and
/// re-splitting the environment at each one bought an env lookup, a split and
/// a fresh allocation per entry and no freshness anyone could observe.  The
/// list is also an input to [`LocateKey`], which needs it computed here anyway.
#[cfg(windows)]
fn windows_pathext_suffixes() -> Vec<String> {
    let pathext = site!(Silent::Pathext, std::env::var_os("PATHEXT"))
        .unwrap_or_else(|| OsStr::new(DEFAULT_PATHEXT).to_os_string());
    pathext
        .as_encoded_bytes()
        .split(|&b| b == b';')
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|ext| ext.trim_start_matches('.').to_owned())
        .collect()
}

/// Mirror the Windows resolver's `%PATHEXT%` fallback, so
/// `locate("python")` finds `python.exe`.  `capability::exec` keeps its
/// own copy of the default list to strip suffixes off command names.
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
            .map(|ext| super::lex::with_appended_suffix(base, ext.as_str())),
    );
    out
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn a_batch_image_is_named_by_its_extension_in_any_case() {
        for name in ["run.bat", r"C:\x\RUN.CMD", "a.b.Bat", ".bat"] {
            assert!(is_batch_image(OsStr::new(name)), "{name}");
        }
        for name in ["run.exe", "bat", "run.bat.exe", "run.batch"] {
            assert!(!is_batch_image(OsStr::new(name)), "{name}");
        }
    }

    /// Win32 trims trailing dots and blanks before it runs a name, so a
    /// batch image spelled with them reaches `cmd.exe` all the same.
    #[test]
    fn a_batch_image_is_named_as_win32_trims_it() {
        for name in ["run.bat.", "run.bat ", "run.CMD. .", r"C:\x\run.bat.."] {
            assert!(is_batch_image(OsStr::new(name)), "{name:?}");
        }
        for name in ["run.exe.", "run.bat.exe ", "bat."] {
            assert!(!is_batch_image(OsStr::new(name)), "{name:?}");
        }
    }

    /// A batch image is one Windows runs by name, so the two lists may only
    /// drift together.
    #[test]
    fn batch_extensions_are_exec_extensions() {
        for ext in WINDOWS_BATCH_EXTENSIONS {
            assert!(WINDOWS_EXEC_EXTENSIONS.contains(ext), "{ext}");
        }
    }
}

/// The memo's tests, on every platform — separate from the walk's `tests`
/// below only because those are written against Unix permission bits.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod memo_tests {
    use super::*;
    use crate::sync::LockExt as _;
    use std::sync::{Mutex, MutexGuard};

    fn cwd_at(dir: &Path) -> CwdPath {
        CwdPath::new(dir.to_path_buf()).expect("a fixture cwd is absolute")
    }

    /// [`GENERATION`] is process-global, so a concurrent sibling's
    /// [`forget_located_commands`] would clear the memo a test is asserting
    /// about.  Poison-tolerant, so a failure does not wedge the rest.
    static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Take the lock and start from a known generation.
    pub(super) fn cache_guard() -> MutexGuard<'static, ()> {
        let guard = CACHE_TEST_LOCK.lock_ignore_poison();
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

        let first = locate("runme", Some(&path_value), None).expect("planted binary must resolve");
        std::fs::remove_file(&planted).unwrap();
        // The file is gone, so a second walk could only answer `None`.
        assert_eq!(locate("runme", Some(&path_value), None), Some(first));
    }

    #[test]
    fn forgetting_re_probes_a_deleted_executable() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let planted = plant(tmp.path(), "runme");
        let path_value = as_path_value(tmp.path());

        assert!(locate("runme", Some(&path_value), None).is_some());
        std::fs::remove_file(&planted).unwrap();
        forget_located_commands();
        assert_eq!(locate("runme", Some(&path_value), None), None);
    }

    #[test]
    fn forgetting_re_probes_a_newly_installed_executable() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let path_value = as_path_value(tmp.path());

        // A remembered miss must not outlive the run's forget, or
        // `cargo install foo` then `foo` still says "not found".
        assert_eq!(locate("runme", Some(&path_value), None), None);
        plant(tmp.path(), "runme");
        forget_located_commands();
        assert!(locate("runme", Some(&path_value), None).is_some());
    }

    #[test]
    fn a_negative_answer_re_probes_when_its_ttl_expires() {
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let path_value = as_path_value(tmp.path());
        let probed = Instant::now();

        assert_eq!(locate_at("runme", Some(&path_value), None, probed), None);
        plant(tmp.path(), "runme");
        assert_eq!(
            locate_at("runme", Some(&path_value), None, probed),
            None,
            "inside the TTL the remembered miss stands"
        );
        assert!(
            locate_at("runme", Some(&path_value), None, probed + NEGATIVE_TTL).is_some(),
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

        let from_a = locate("runme", Some(&as_path_value(a.path())), None).unwrap();
        let from_b = locate("runme", Some(&as_path_value(b.path())), None).unwrap();
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

        assert!(locate("runme", Some("./bin"), Some(&cwd_at(a.path()))).is_some());
        // Same name and same `PATH`; only the anchor differs.
        assert_eq!(
            locate("runme", Some("./bin"), Some(&cwd_at(b.path()))),
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
            locate("runme", Some(&path_value), None)
        });
        let with = crate::test_env::with_var("PATHEXT", Some(".BAT"), || {
            locate("runme", Some(&path_value), None)
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

        assert!(locate("./runme", None, Some(&cwd_at(tmp.path()))).is_some());
        std::fs::remove_file(&planted).unwrap();
        // No `forget`, and still a fresh answer.
        assert_eq!(locate("./runme", None, Some(&cwd_at(tmp.path()))), None);
    }
}

/// The Windows resolver's own rules: `%PATHEXT%` suffixes append to the name
/// the user wrote, and never stand in for one they did.
#[cfg(test)]
#[cfg(windows)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod windows_tests {
    use super::*;

    /// `%PATHEXT%`'s fallback and the name-key strip list are twin copies of
    /// one fact; they may only drift together.
    #[test]
    fn name_key_extensions_agree_with_the_resolver_default_pathext() {
        let from_pathext: Vec<String> = DEFAULT_PATHEXT
            .split(';')
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect();
        assert_eq!(from_pathext, WINDOWS_EXEC_EXTENSIONS);
    }

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
                locate("build.ps1", Some(&path_value), None),
                None,
                "`build.ps1` names a PowerShell script, not whatever build.exe is first on PATH",
            );
            forget_located_commands();
            assert!(locate("build", Some(&path_value), None).is_some());
        });
    }
}

#[cfg(test)]
mod path_name_tests {
    use super::*;

    /// `/` makes a path under both rules; `\` only under Windows, so a Posix
    /// `a\b` is a bare name for `PATH`.
    #[test]
    fn a_separator_under_the_rules_makes_a_command_name_a_path() {
        for (name, posix, windows) in [
            ("git", false, false),
            ("./tool", true, true),
            ("/bin/sh", true, true),
            (r"a\b", false, true),
            (r".\tool", false, true),
            (r"C:\Users\tool", false, true),
        ] {
            assert_eq!(
                names_a_path_for(name, PathRules::Posix),
                posix,
                "{name} posix"
            );
            assert_eq!(
                names_a_path_for(name, PathRules::Windows),
                windows,
                "{name} windows"
            );
        }
    }
}

#[cfg(test)]
mod name_key_tests {
    use super::*;

    #[test]
    fn name_key_off_windows_is_the_name() {
        assert_eq!(name_key_on("Git.exe", PathRules::Posix), "Git.exe");
    }

    #[test]
    fn windows_name_key_folds_case_and_strips_an_executable_extension() {
        for name in [
            "git", "GIT", "git.exe", "Git.EXE", "git.cmd", "git.com", "git.bat",
        ] {
            assert_eq!(name_key_on(name, PathRules::Windows), "git", "{name}");
        }
        assert_eq!(name_key_on("git.tool", PathRules::Windows), "git.tool");
        assert_eq!(name_key_on("gitk.exe", PathRules::Windows), "gitk");
    }

    /// A deny holds a command name under every spelling, as it holds a path.
    #[test]
    fn the_command_name_key_folds_case_and_normalisation() {
        assert_eq!(command_name_key("SH"), command_name_key("sh"));
        assert_eq!(
            command_name_key("caf\u{e9}"),
            command_name_key("cafe\u{301}")
        );
        assert_ne!(command_name_key("shx"), command_name_key("sh"));
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cwd_at(dir: &Path) -> CwdPath {
        CwdPath::new(dir.to_path_buf()).expect("a fixture cwd is absolute")
    }

    fn touch(p: &Path, mode: u32) {
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn locate_folds_the_dots_out_of_an_anchored_path() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("runme"), 0o755);
        // Folded, not merely joined: the answer is what the OS sandbox
        // profile matches literally, and `/tmp/x/./runme` is not `/tmp/x/runme`.
        let hit = locate("./runme", None, Some(&cwd_at(tmp.path()))).unwrap();
        assert_eq!(hit, tmp.path().join("runme"));
    }

    #[test]
    fn path_dirs_drops_empties_anchors_relatives_and_keeps_absolutes() {
        let dirs = path_dirs("/usr/bin::./a/../b:/abs", Some(&cwd_at(Path::new("/w"))));
        assert_eq!(
            dirs,
            [Path::new("/usr/bin"), Path::new("/w/b"), Path::new("/abs")]
        );
    }

    #[test]
    fn a_relative_path_entry_is_read_only_against_a_cwd() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bin")).unwrap();
        touch(&tmp.path().join("bin/tool"), 0o755);
        assert_eq!(locate("tool", Some("bin"), None), None);
        assert_eq!(
            locate("tool", Some("bin"), Some(&cwd_at(tmp.path()))),
            Some(tmp.path().join("bin/tool"))
        );
        let path = format!("bin:{}", tmp.path().display());
        assert_eq!(path_dirs(path, None), [tmp.path()]);
    }

    #[test]
    fn an_empty_element_is_dropped_by_path_entries() {
        let entries: Vec<_> = path_entries(OsStr::new(":/a::")).collect();
        assert_eq!(entries, [Path::new("/a")]);
    }

    /// The 126 case, and the only platform that has one: the presence half
    /// answers where the executable half declined, off the same walk.
    #[test]
    fn search_reports_found_not_executable_on_the_same_walk() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("noexec"), 0o644);
        let path_value = tmp.path().to_str().unwrap();

        assert_eq!(locate("noexec", Some(path_value), None), None);
        match search("noexec", Some(path_value), None) {
            PathSearch::FoundNotExecutable(p) => assert_eq!(p, tmp.path().join("noexec")),
            other => panic!("expected FoundNotExecutable, got {other:?}"),
        }
    }
}
