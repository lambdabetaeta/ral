//! `PATH` search: a bare command name's candidates, each walked.
//!
//! `$PATH` is a list of directories tried in turn, so a candidate is a
//! directory joined with the name (on Windows, with each `%PATHEXT%` suffix),
//! reached by [`walk()`](crate::path::walk()) as any other spelling is.  That is a walk per `PATH`
//! entry, so where the walks find the first program is remembered for the
//! extent of a run; [`LOCATED`] argues what that costs in freshness.
//! Dispatch arrives via `shell::exec::command`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::ops::ControlFlow;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::lex::{anchor_on, fold_dots};
#[cfg(windows)]
use super::site::Silent;
use super::{CwdPath, LeafMode, LexicalPath, Located, walk};
#[cfg(windows)]
use crate::site::site;

/// A command's file as a launch reaches it: its spelling, and the walk of it.
pub(crate) struct Candidate {
    /// What the program sees as `argv[0]`.
    pub(crate) arg0: PathBuf,
    pub(crate) spelled: LexicalPath,
    /// Links followed, as `execve` follows them.
    pub(crate) walked: io::Result<Located>,
}

impl Candidate {
    /// `arg0` walked; `None` where it is not absolute, so names nothing.
    pub(crate) fn walk(arg0: PathBuf) -> Option<Self> {
        let spelled = LexicalPath::absolute(&arg0)?;
        let walked = walk(&spelled, LeafMode::Follow);
        Some(Self {
            arg0,
            spelled,
            walked,
        })
    }

    /// For ordering alone: no judgment has read it.
    fn holds_program(&self) -> bool {
        self.walked.as_ref().is_ok_and(Located::holds_program)
    }
}

/// The first program on `path_value` for the bare `name`: what a launch
/// under no grant runs.
///
/// Relative entries are read against `cwd`.  Remembered for the current run
/// ([`LOCATED`]), and a remembered answer is not walked.
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
    let (key, candidates) = query(name, path_value?, cwd);
    match recall(&key, now) {
        Recalled::Remembered(answer) => answer,
        Recalled::Cold => fold(key, &candidates, None, now, |candidate| {
            if candidate.holds_program() {
                ControlFlow::Break(candidate.arg0)
            } else {
                ControlFlow::Continue(())
            }
        })
        .ok(),
    }
}

/// The first of bare `name`'s `PATH` candidates `judge` takes, each walked
/// at most once; else what `judge` made of each candidate it passed over,
/// in `PATH` order.  `judge` takes only a program
/// ([`Located::holds_program`]): the remembered one is tried first, on the
/// strength of no earlier candidate being one.
pub(crate) fn search<T, C>(
    name: &str,
    path_value: Option<&str>,
    cwd: Option<&CwdPath>,
    judge: impl FnMut(Candidate) -> ControlFlow<T, C>,
) -> Result<T, Vec<C>> {
    let Some(path_value) = path_value else {
        return Err(Vec::new());
    };
    let now = Instant::now();
    let (key, candidates) = query(name, path_value, cwd);
    let first = match recall(&key, now) {
        Recalled::Remembered(Some(hit)) => candidates.iter().position(|c| *c == hit),
        Recalled::Remembered(None) | Recalled::Cold => None,
    };
    fold(key, &candidates, first, now, judge)
}

/// `candidates` walked and judged, `first` before the rest, each verdict
/// kept at its place in `PATH` order.  The memo then learns the first
/// program, where the walks decide it: every candidate before it walked.
fn fold<T, C>(
    key: LocateKey,
    candidates: &[PathBuf],
    first: Option<usize>,
    now: Instant,
    mut judge: impl FnMut(Candidate) -> ControlFlow<T, C>,
) -> Result<T, Vec<C>> {
    // Per candidate: unwalked, or whether its walk found a program.
    let mut programs = vec![None; candidates.len()];
    let mut passed: Vec<Option<C>> = candidates.iter().map(|_| None).collect();
    let rest = (0..candidates.len()).filter(|at| Some(*at) != first);
    let mut taken = None;
    for at in first.into_iter().chain(rest) {
        let candidate = Candidate::walk(candidates[at].clone());
        programs[at] = Some(candidate.as_ref().is_some_and(Candidate::holds_program));
        match candidate.map(&mut judge) {
            Some(ControlFlow::Break(it)) => {
                debug_assert_eq!(programs[at], Some(true), "`judge` takes only a program");
                taken = Some(it);
                break;
            }
            Some(ControlFlow::Continue(verdict)) => passed[at] = Some(verdict),
            None => {}
        }
    }
    match programs
        .iter()
        .zip(candidates)
        .find(|(walked, _)| **walked != Some(false))
    {
        None => remember(key, None, now),
        Some((Some(true), hit)) => remember(key, Some(hit.clone()), now),
        Some(_) => {}
    }
    taken.ok_or_else(|| passed.into_iter().flatten().collect())
}

/// The memo's key for bare `name` on `path_value` read against `cwd`, and
/// the files the name may be, in `PATH` order: each [`path_dirs`] entry
/// joined with it, on Windows with each `%PATHEXT%` suffix.
fn query(name: &str, path_value: &str, cwd: Option<&CwdPath>) -> (LocateKey, Vec<PathBuf>) {
    let dirs = path_dirs(path_value, cwd);
    #[cfg(windows)]
    let suffixes = windows_pathext_suffixes();
    #[cfg(windows)]
    let candidates = dirs
        .iter()
        .flat_map(|dir| windows_command_candidates(&dir.join(name), &suffixes))
        .collect();
    #[cfg(not(windows))]
    let candidates = dirs.iter().map(|dir| dir.join(name)).collect();
    let key = LocateKey {
        name: name.to_owned(),
        path_value: path_value.to_owned(),
        cwd: cwd.map(|c| c.as_path().to_path_buf()),
        #[cfg(windows)]
        pathext: suffixes.join(";"),
    };
    (key, candidates)
}

/// The first program named `name` in `dirs`, in order and unmemoised, for a
/// caller holding directories rather than a `PATH` string.
#[cfg(target_os = "linux")]
pub(crate) fn locate_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .filter_map(|dir| Candidate::walk(dir.join(name)))
        .find(Candidate::holds_program)
        .map(|found| found.arg0)
}

/// How long a *negative* answer is trusted, inside the run that took it:
/// short enough that `cargo install foo; foo` in one dispatch works.
const NEGATIVE_TTL: Duration = Duration::from_secs(1);

/// Bumped by [`forget_located_commands`]; a thread's memo is stale the moment
/// its stamp differs.  Process-global while the map is per-thread, so a bump
/// made anywhere is honoured everywhere — conservatively, since a thread that
/// remembered nothing loses nothing by clearing.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Answer {
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
    entries: HashMap<LocateKey, Answer>,
}

thread_local! {
    /// Where the walks found a name's first program, paid once per run
    /// rather than once per name.
    ///
    /// Thread-local and lock-free, because a run's evaluation is
    /// single-threaded on the calling thread.  It hangs off no `Shell` and no
    /// `Context` on purpose: its callers, `shell::exec::command`'s
    /// resolution and `authority::grant`'s decode of a bare key, hold no `Shell`.
    ///
    /// A newly installed or newly deleted executable is therefore invisible
    /// for at most the remainder of the current top-level run — one submitted
    /// line in the REPL, one script in batch, one dispatch in the engine — and
    /// nothing survives a run boundary, which is fresher than bash's `hash`
    /// table.  Within a run a `None` older than [`NEGATIVE_TTL`] is walked
    /// again while positives ride the generation alone, because a stale miss
    /// is a wrong answer where a stale hit costs [`search`] one walk: it is no
    /// program there, and the search goes on in `PATH` order.
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
        cache.entries.insert(key, Answer { hit, probed: now });
    });
}

/// Forget every remembered `PATH` answer, on every thread: the next
/// [`locate`] or [`search`] walks every candidate again.
///
/// The top-level run calls this, so a caller needs it only to admit a
/// *mid-run* change to the filesystem — planting a binary and resolving it in
/// the same dispatch.  What staleness that leaves is argued at [`LOCATED`].
pub fn forget_located_commands() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The one directory list behind [`locate`] and [`search`]; relative
/// entries (`./bin`) anchor to `cwd`, and with no cwd are dropped, as a
/// grant's `path` selector and the bwrap search read `PATH`.
///
/// Folded, since a resolved directory travels on to the OS sandbox profile
/// via [`SandboxProjection::of`](crate::authority::sandbox::SandboxProjection::of),
/// which matches literally: the `/work/./bin/git` an unfolded `./bin` yields
/// is covered by no `/work/bin/` the profile names.
pub fn path_dirs(path_value: impl AsRef<OsStr>, cwd: Option<&CwdPath>) -> Vec<PathBuf> {
    path_entries(path_value.as_ref())
        .filter_map(|dir| anchor_on(cwd, &dir))
        .map(|dir| fold_dots(&dir))
        .collect()
}

/// The entries of a `PATH`-style list, in order and as written, split on the
/// host's separator (`:`, or `;` on Windows): the one place such a list is
/// split.
///
/// An **empty element is dropped, on every platform**.  POSIX reads one as the
/// cwd — the forty-year-old implicit-`.`-on-`PATH` foot-gun — and on Windows a
/// trailing `;` is ubiquitous noise, so honouring it would put every file of
/// every directory the user `cd`s into on the search list; off Unix, where
/// any file is a program ([`Located::holds_program`]), that is every file
/// outright.  A user who wants the cwd searched writes `.`, which
/// [`path_dirs`] honours deliberately.
pub fn path_entries(path_value: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path_value).filter(|dir| !dir.as_os_str().is_empty())
}

/// The resolver's fallback when `%PATHEXT%` is unset;
/// [`WINDOWS_EXEC_EXTENSIONS`](super::lex::WINDOWS_EXEC_EXTENSIONS) is its
/// stripped twin, the agreement pinned below.
#[cfg(windows)]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The Windows resolver's `%PATHEXT%` suffixes, leading dots stripped, with
/// the resolver's own default when the variable is unset.  An entry that is
/// not Unicode is dropped, since a suffix is appended as text.
///
/// Read once per candidate list, not per directory; also an input to [`LocateKey`].
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
/// `locate("python")` finds `python.exe`.  `authority::policy::exec` keeps its
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

/// `file` with each executable extension added, walked. Used to suggest the
/// extension a user left off a path that names nothing.
#[cfg(windows)]
pub(crate) fn with_exec_extensions(file: &Path) -> impl Iterator<Item = (&'static str, Candidate)> {
    super::lex::WINDOWS_EXEC_EXTENSIONS
        .iter()
        .filter_map(|&ext| {
            Candidate::walk(super::lex::with_appended_suffix(file, ext)).map(|found| (ext, found))
        })
}

/// The memo's tests, on every platform — separate from the walk's `tests`
/// below only because those are written against Unix permission bits.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod memo_tests {
    use super::*;
    use crate::sync::LockExt as _;
    use std::path::Path;
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

    /// Whatever the one executable predicate demands: the `+x` bit on Unix,
    /// and nothing off it, where any file is a program.
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
        use crate::host::test_support::with_var;
        let _guard = cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        make_executable(&tmp.path().join("runme.bat"));
        let path_value = as_path_value(tmp.path());

        // No `forget` between the two: `%PATHEXT%` is in the key itself.
        let without = with_var("PATHEXT", Some(".EXE"), || {
            locate("runme", Some(&path_value), None)
        });
        let with = with_var("PATHEXT", Some(".BAT"), || {
            locate("runme", Some(&path_value), None)
        });
        assert_eq!(without, None);
        assert!(with.is_some(), "got {with:?}");
    }
}

/// The Windows resolver's own rules: `%PATHEXT%` suffixes append to the name
/// the user wrote, and never stand in for one they did.
#[cfg(test)]
#[cfg(windows)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod windows_tests {
    use super::*;
    use crate::host::test_support::with_var;
    use crate::path::lex::WINDOWS_EXEC_EXTENSIONS;

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

        with_var("PATHEXT", Some(".EXE"), || {
            assert_eq!(
                locate("build.ps1", Some(&path_value), None),
                None,
                "`build.ps1` names a PowerShell script, not whatever build.exe is first on PATH",
            );
            forget_located_commands();
            assert!(locate("build", Some(&path_value), None).is_some());
        });
    }

    /// `C:bin` on a `PATH` of ral's cwd drive is read against ral's cwd, not
    /// left for the process's per-drive directory to answer: `Path::join`
    /// would hand back `C:bin` unchanged.
    #[test]
    fn a_drive_relative_entry_on_the_cwd_drive_anchors_to_the_cwd() {
        let cwd = CwdPath::new(PathBuf::from(r"C:\w\p")).unwrap();
        assert_eq!(
            path_dirs(r"C:bin;C:.\tool", Some(&cwd)),
            [PathBuf::from(r"C:\w\p\bin"), PathBuf::from(r"C:\w\p\tool")],
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

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

    /// Dirs holding, in order, a non-executable `tool`, a directory `tool`,
    /// and two executable ones; the third dir's is the first hit.
    fn shadowed_tool() -> (tempfile::TempDir, Vec<PathBuf>) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs: Vec<PathBuf> = ["noexec", "dir", "first", "second"]
            .map(|d| tmp.path().join(d))
            .into();
        for d in &dirs {
            std::fs::create_dir(d).unwrap();
        }
        touch(&dirs[0].join("tool"), 0o644);
        std::fs::create_dir(dirs[1].join("tool")).unwrap();
        touch(&dirs[2].join("tool"), 0o755);
        touch(&dirs[3].join("tool"), 0o755);
        (tmp, dirs)
    }

    #[test]
    fn locate_takes_the_first_executable_file_in_path_order() {
        let _guard = super::memo_tests::cache_guard();
        let (_tmp, dirs) = shadowed_tool();
        let path = std::env::join_paths(&dirs).unwrap();
        assert_eq!(
            locate("tool", path.to_str(), None),
            Some(dirs[2].join("tool"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn locate_in_takes_the_first_executable_file_in_the_given_order() {
        let (_tmp, dirs) = shadowed_tool();
        assert_eq!(locate_in("tool", dirs.clone()), Some(dirs[2].join("tool")));
        assert_eq!(locate_in("tool", dirs[..2].to_vec()), None);
    }

    /// A bump made on one thread clears the memo of another.
    #[test]
    fn forgetting_on_another_thread_clears_this_threads_memo() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        let planted = tmp.path().join("runme");
        touch(&planted, 0o755);
        let path = tmp.path().to_str().unwrap();

        assert!(locate("runme", Some(path), None).is_some());
        std::fs::remove_file(&planted).unwrap();
        std::thread::spawn(forget_located_commands).join().unwrap();
        assert_eq!(locate("runme", Some(path), None), None);
    }

    /// A search hands its judge a file `locate` passes over as no program.
    #[test]
    fn a_search_hands_the_judge_a_non_executable_file() {
        let _guard = super::memo_tests::cache_guard();
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("noexec"), 0o644);
        let path_value = tmp.path().to_str();

        assert_eq!(locate("noexec", path_value, None), None);
        let passed = search("noexec", path_value, None, |candidate| {
            ControlFlow::<(), _>::Continue(candidate.arg0)
        });
        assert_eq!(passed, Err(vec![tmp.path().join("noexec")]));
    }

    /// The remembered program is walked first, and its verdict keeps its
    /// place in `PATH` order.
    #[test]
    fn a_search_tries_the_remembered_program_first() {
        let (_tmp, dirs) = shadowed_tool();
        let path = std::env::join_paths(&dirs).unwrap();
        let (key, candidates) = query("tool", path.to_str().unwrap(), None);
        let mut walked = Vec::new();
        let passed = fold(key, &candidates, Some(2), Instant::now(), |candidate| {
            walked.push(candidate.arg0.clone());
            ControlFlow::<(), _>::Continue(candidate.arg0)
        });
        assert_eq!(walked, [2, 0, 1, 3].map(|at| candidates[at].clone()));
        assert_eq!(passed, Err(candidates));
    }

    /// Whatever its judge takes, a search's walks teach the memo where the
    /// first program is.
    #[test]
    fn a_search_remembers_the_first_program_its_walks_found() {
        let _guard = super::memo_tests::cache_guard();
        let (_tmp, dirs) = shadowed_tool();
        let path = std::env::join_paths(&dirs).unwrap();
        let path = path.to_str();

        let taken = search("tool", path, None, |candidate| {
            if candidate.arg0.starts_with(&dirs[3]) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(taken, Ok(()));
        std::fs::remove_file(dirs[2].join("tool")).unwrap();
        // The file is gone, so only the memo can answer with it.
        assert_eq!(locate("tool", path, None), Some(dirs[2].join("tool")));
    }
}
