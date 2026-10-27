//! The apps and their scratch and XDG directories, and the time/slug helpers
//! those share.

use ral_core::Shell;
#[cfg(any(test, feature = "test-util"))]
use ral_core::own::TestDir;
use ral_core::own::{Entry, OwnDir, OwnError};
use ral_core::path::{
    GuestPath, Kind, NotUtf8, Visibility, XdgKind, app_dir, join_slash, project_slug,
    spelled_parent,
};
use std::cmp::Ordering;
use std::convert::Infallible;
use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The name of a run's advisory lock file.
const RUN_LOCK: &str = "run.lock";

/// The file a session's records are kept in, in the session's directory: what
/// makes a run [resumable](resume_candidates).
pub const RECORD: &str = "record.jsonl";

/// The engine reads it by name ([`crate::boot::engine_root`]); it is the protocol's,
/// not a product's.
pub const SCRATCH_VAR: &str = "EXARCH_SCRATCH";

/// The run's advisory write lock, held as long as this process lives.
///
/// The lock belongs to the open file the value holds, which the OS releases
/// when the process ends, by whatever means it ends: that is what makes the
/// lock an honest answer to "is anyone still using this?".
pub struct RunLock {
    _lock: File,
}

impl RunLock {
    /// Take `run`'s lock without waiting.
    ///
    /// # Errors
    /// `WouldBlock` while another exarch holds it; the open's.
    pub fn try_acquire(run: &OwnDir) -> io::Result<Self> {
        let lock = run.open_write(RUN_LOCK)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self { _lock: lock }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{run} is in use by another exarch"),
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

/// Per-session scratch directory, named to the agent by [`SCRATCH_VAR`].
///
/// Everything ephemeral the agent scribbles belongs here, because
/// `reasonable` denies writes to the user's real cache dirs.
///
/// A scratch holds whole tool caches — see [`TOOL_HOMES`] — so it lives under
/// the app's cache directory, `$XDG_CACHE_HOME/<app>/scratch/`, on disk and
/// beside no one else's files.  Nothing sweeps it but exarch: a session holds
/// a lock beside its scratch for as long as it runs, and [`Scratch::new`]
/// deletes the scratches no lock answers for.
pub struct Scratch {
    dir: OwnDir,
    /// Where it is, as text ([`Self::spelling`]), and what [`Self::env`]
    /// names: both settled once at creation, since each fails only for a
    /// directory that is not Unicode, which nothing later should have to ask.
    spelling: String,
    env: Vec<(String, String)>,
    #[cfg_attr(
        not(any(test, feature = "test-util")),
        expect(
            dead_code,
            reason = "held for what its Drop does; only a test double reads it, for the room beside it"
        )
    )]
    hold: Hold,
}

/// What answers for a scratch's life, and so what ends it.
///
/// A session's lock is read by [`reap_unheld`] only after a session dies
/// without its drop; a test's guard is read by nobody, because the test's own
/// end deletes the directory.
#[expect(
    dead_code,
    reason = "each variant is held for what its Drop does (deleting the session's scratch, deleting the test's directory) and so is never read"
)]
enum Hold {
    Session(SessionHold),
    #[cfg(any(test, feature = "test-util"))]
    Test(TestDir),
}

/// A live session's scratch and the lock that says so, side by side.
struct SessionHold {
    root: OwnDir,
    name: String,
    lock_name: String,
    lock: Option<File>,
}

impl Drop for SessionHold {
    /// The directory, then the lock: closed before it is unlinked, because
    /// Windows will not unlink an open file.
    fn drop(&mut self) {
        let _ = self.root.remove(&self.name);
        drop(self.lock.take());
        let _ = self.root.remove(&self.lock_name);
    }
}

/// The lock file's name is its scratch's with this appended, so each stands
/// beside the other and either names the other.
const LOCK_SUFFIX: &str = ".lock";

/// What a tool home takes under a scratch: a subdirectory, or a command-line
/// flag whose value is one.
#[derive(Clone, Copy)]
enum Home {
    Dir(&'static str),
    Flag(&'static str, &'static str),
}

impl Home {
    /// The value, given how a subdirectory of the scratch is spelled.
    fn value<E>(self, under: impl Fn(&'static str) -> Result<String, E>) -> Result<String, E> {
        match self {
            Self::Dir(sub) => under(sub),
            Self::Flag(prefix, sub) => under(sub).map(|dir| format!("{prefix}{dir}")),
        }
    }
}

/// Every cache and temp file lives in the scratch: each variable is
/// replaced, whatever the host inherited, by a value under the scratch.
const TOOL_HOMES: &[(&str, Home)] = &[
    ("TMPDIR", Home::Dir("tmp")),
    // Caches only: RUSTUP_HOME holds what runs, so it stays the host's.
    ("XDG_CACHE_HOME", Home::Dir("xdg-cache")),
    ("CARGO_HOME", Home::Dir("cargo")),
    ("GRADLE_USER_HOME", Home::Dir("gradle")),
    ("GOPATH", Home::Dir("go")),
    ("GOMODCACHE", Home::Dir("go/pkg/mod")),
    ("npm_config_cache", Home::Dir("npm-cache")),
    ("GOCACHE", Home::Dir("go-build")),
    ("PIP_CACHE_DIR", Home::Dir("pip")),
    ("POETRY_CACHE_DIR", Home::Dir("poetry")),
    ("pnpm_config_store_dir", Home::Dir("pnpm-store")),
    ("npm_config_store_dir", Home::Dir("pnpm-store")),
    ("BUN_INSTALL_CACHE_DIR", Home::Dir("bun")),
    ("YARN_GLOBAL_FOLDER", Home::Dir("yarn-berry")),
    ("MAVEN_ARGS", Home::Flag("-Dmaven.repo.local=", "maven")),
];

/// Settings the confinement forces on a tool, as against relocating one.
/// Cargo's bundled libgit2 speaks TLS through `SecureTransport`, whose handshake
/// reaches securityd — the keychain service no profile admits — and reports the
/// denial as `ssl handshake -9808`, which cargo then blames on a missing
/// revision.  The `git` binary's TLS needs no such service, so cargo fetches
/// through it.
const CONFINED_TOOL_SETTINGS: &[(&str, &str)] = &[("CARGO_NET_GIT_FETCH_WITH_CLI", "true")];

impl Scratch {
    /// Create this session's scratch under a lock that says it is live, and
    /// delete the scratches whose sessions are over.
    ///
    /// The lock file is made first and the directory takes its name from it,
    /// so a scratch is never visible unheld: any directory another session can
    /// find already has a held lock beside it.
    ///
    /// # Errors
    /// Returns `Err` if the cache directory, the lock file or the directory
    /// cannot be made, if another session's sweep takes three locks in a row,
    /// or if the directory's path is not valid Unicode, since the environment
    /// that names it is text.
    ///
    /// # Panics
    /// Never: the lock is made with [`LOCK_SUFFIX`] appended, which is what
    /// names its scratch.
    pub fn new(app: App) -> io::Result<Self> {
        Self::in_root(&app.own(XdgKind::Cache)?.make_dir("scratch")?)
    }

    /// [`Self::new`] over `root`, the directory the app's scratches share.
    fn in_root(root: &OwnDir) -> io::Result<Self> {
        let (lock_name, lock) = fresh_lock(root)?;
        let name = lock_name
            .strip_suffix(LOCK_SUFFIX)
            .expect("a lock is named with the suffix it was made with")
            .to_owned();
        // Held from here, so a failure below deletes what was made.
        let hold = SessionHold {
            root: root.clone(),
            name,
            lock_name,
            lock: Some(lock),
        };
        let dir = root.make_dir(&hold.name)?;
        reap_unheld(root, &hold.lock_name);
        Self::made(dir, Hold::Session(hold))
    }

    /// The scratch `dir` makes.
    fn made(dir: OwnDir, hold: Hold) -> io::Result<Self> {
        dir.make_dir("tmp")?;
        let unicode = |e: NotUtf8| io::Error::new(io::ErrorKind::InvalidData, e);
        let spelling = ral_core::path::utf8(dir.path())
            .map_err(unicode)?
            .to_owned();
        let env = scratch_env(dir.path()).map_err(unicode)?;
        Ok(Self {
            dir,
            spelling,
            env,
            hold,
        })
    }

    /// Where it is, spelled as the agent's environment names it.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where it is, as text: the spelling a grant's `scratch` place anchors to
    /// and the value of the scratch variable.
    pub fn spelling(&self) -> &str {
        &self.spelling
    }

    /// A scratch for one test, deleted when the returned value falls.  A test
    /// process ends normally, so the guard suffices and no lock is taken.
    ///
    /// `tag` names the test, and only so that the rare directory outliving a
    /// killed test run says which test left it.
    ///
    /// Public and hidden because synod's tests need the same scaffolding.
    ///
    /// # Errors
    /// Returns `Err` if the directory cannot be created, or if its path is
    /// not valid Unicode.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn for_test(app: App, tag: &str) -> io::Result<Self> {
        let root = OwnDir::for_test(&format!("{}-{tag}", app.name()))?;
        let dir = root.make_dir("scratch")?;
        Self::made(dir, Hold::Test(root))
    }

    /// A directory beside this test scratch rather than inside it, deleted
    /// along with it.
    ///
    /// Beside, because a real session's log and scratch are separate roots,
    /// and `disk_warning` sums the two: nested, the log would be counted
    /// twice and a test double would measure what no session ever does.
    ///
    /// # Errors
    /// Returns `Err` on a session's scratch, which has no room beside it, or
    /// if the directory cannot be created.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn test_sibling(&self, name: &str) -> io::Result<OwnDir> {
        let Hold::Test(root) = &self.hold else {
            return Err(io::Error::other(
                "only a test scratch has room beside it; a session's scratch stands alone",
            ));
        };
        Ok(root.make_dir(name)?)
    }

    /// Seed [`SCRATCH_VAR`], the [`TOOL_HOMES`] and
    /// [`CONFINED_TOOL_SETTINGS`] into `shell`, overriding whatever was
    /// inherited: a `CARGO_HOME` under `~/.cargo` lands outside reasonable's
    /// write set, so the sandbox is the trust boundary, not the environment
    /// exarch was launched in.  One seeder for all three, because the seat's
    /// test double mirrors this call and a second site would drift from it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn install_into(&self, shell: &mut Shell) {
        for (name, value) in self.env() {
            seed_var(shell, &name, &value);
        }
    }

    /// What this scratch names to the agent: [`SCRATCH_VAR`], the
    /// [`TOOL_HOMES`] and [`CONFINED_TOOL_SETTINGS`], as the pairs an `Attach`
    /// names.
    pub fn env(&self) -> Vec<(String, String)> {
        self.env.clone()
    }
}

/// What a host scratch at `dir` names to the agent: each path native, joined
/// as the host joins.
///
/// # Errors
/// [`NotUtf8`] when `dir` is not Unicode: an environment value is text, and a
/// lossy one would name a different directory.
fn scratch_env(dir: &Path) -> Result<Vec<(String, String)>, NotUtf8> {
    let text = |path: &Path| ral_core::path::utf8(path).map(str::to_owned);
    env_under(text(dir)?, |sub| text(&join_slash(dir, sub)))
}

/// What a scratch at `dir` inside a Posix guest names to the agent.
///
/// `dir` is the guest's path, which the host cannot hold as a `PathBuf`, so
/// every home joins under it as the guest joins, with `/`, whatever the host.
pub(crate) fn guest_scratch_env(dir: &GuestPath) -> Vec<(String, String)> {
    let Ok(env) = env_under(dir.as_str().to_owned(), |sub| {
        Ok::<_, Infallible>(dir.join(sub).as_str().to_owned())
    });
    env
}

/// The scratch var, the tool homes under it, and the confined tool settings,
/// for a scratch spelled `scratch` whose subdirectories `under` spells, or
/// fails to spell: one table for a host's own [`Scratch`] and for a guest's.
fn env_under<E>(
    scratch: String,
    under: impl Fn(&'static str) -> Result<String, E>,
) -> Result<Vec<(String, String)>, E> {
    let homes = TOOL_HOMES
        .iter()
        .map(|(var, home)| home.value(&under).map(|value| ((*var).to_string(), value)));
    let settings = CONFINED_TOOL_SETTINGS
        .iter()
        .map(|(var, value)| Ok(((*var).to_string(), (*value).to_string())));
    std::iter::once(Ok((SCRATCH_VAR.to_owned(), scratch)))
        .chain(homes)
        .chain(settings)
        .collect()
}

/// A lock file made now under a fresh name, and held.  Another session's
/// sweep can open it in the instant between its making and its taking, and
/// then deletes it as abandoned: that name is the sweep's, and another is
/// made, three times at most.
///
/// # Errors
/// The make's or the lock's; `WouldBlock` where three names in a row were
/// taken by sweeps.
fn fresh_lock(root: &OwnDir) -> io::Result<(String, File)> {
    for _ in 0..3 {
        let (name, file) = root.create_fresh("", LOCK_SUFFIX, Visibility::Private)?;
        match file.try_lock() {
            Ok(()) => return Ok((name, file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "could not hold a scratch lock in {root}: another exarch's sweep took three in a \
             row; start again"
        ),
    ))
}

/// Delete the scratches no live session answers for.
///
/// Every scratch has a lock file beside it, held for as long as its session
/// runs, so one question settles ownership: can the lock be taken?  The OS
/// releases it however the session ended — a clean exit, a panic, a `SIGKILL`,
/// a power cut — and no lock outlives a reboot, so this single test covers
/// every way a session can fail to tidy up after itself.  Nothing here reads a
/// pid or a boot identifier: each of those only approximates what the lock
/// states exactly.
///
/// `mine` is skipped by name rather than trusted to fail the test.  `flock`
/// keys on the open file description, so a second attempt from this same
/// process does answer "held": but only on the platforms that have `flock`,
/// and a scratch must not depend on that to survive its own reaping.
///
/// Failures are silent throughout: a scratch that resists deletion is a
/// nuisance, never a reason to refuse the session it was made for.
fn reap_unheld(root: &OwnDir, mine: &str) {
    let Ok(entries) = root.list() else { return };
    for Entry { name: lock, .. } in entries {
        let Some(scratch) = lock.strip_suffix(LOCK_SUFFIX) else {
            continue;
        };
        if lock == mine {
            continue;
        }
        // Opened, never created: a lock file that has just been deleted names
        // a scratch that is already somebody else's business.
        let Ok(file) = root.open(&lock) else { continue };
        if file.try_lock().is_err() {
            continue;
        }
        // Closed before the deletion, because Windows will not unlink a file
        // this process still holds open.  Another session may take it in
        // between and delete the same scratch; one of the two then fails
        // harmlessly, and no *live* session can be the one that takes it,
        // since a new session always makes a new name.
        drop(file);
        let _ = root.remove(scratch);
        let _ = root.remove(&lock);
    }
}

/// Which product's directories these are.
///
/// Exarch and synod are two applications over one engine, and neither may
/// reach into the other's logs or persisted model selection; the name is
/// the only thing that varies, so it is the whole type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct App(&'static str);

/// Exarch's own directories.
pub const EXARCH: App = App::new("exarch");

/// Synod's, named here rather than in synod.
///
/// An agent exarch launches must not read *any* product's credential store, so
/// [`APPS`] has to be able to say which directories those are
/// ([`crate::provider::credential_files`]).
pub const SYNOD: App = App::new("synod");

/// Every product over this engine.  A grant carves all of them out at once —
/// one app's agent reading another's keys is the same leak either way.
pub const APPS: &[App] = &[EXARCH, SYNOD];

impl App {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// This app's directory under an XDG base, `$XDG_<kind>_HOME/<app>/`, as
    /// [`ral_core::path::app_dir`] spells it: a name for the consumers that
    /// take one — a deny, a skill root — and for none that writes.  `None`
    /// where the host names no `kind` base.
    #[must_use]
    pub fn xdg_dir(self, kind: XdgKind) -> Option<PathBuf> {
        app_dir(kind, self.0)
    }

    /// This app's directory under the host's `kind` base, held:
    /// `$XDG_<kind>_HOME/<app>/`.
    ///
    /// # Errors
    /// [`OwnDir::xdg`]'s.
    pub fn own(self, kind: XdgKind) -> Result<OwnDir, OwnError> {
        OwnDir::xdg(self.0, kind)
    }

    /// `$XDG_STATE_HOME/<app>/<project>/`, held and made where absent,
    /// `<project>` being [`ral_core::path::project_slug`] of the absolute
    /// `cwd`: a readable part plus a 16-hex identity of its exact OS bytes.
    /// Holds `provider::state`'s model selection and the per-run logs, so a
    /// project's state is one directory keyed by where it was launched, never
    /// scattered into cwd.
    ///
    /// # Errors
    /// [`Self::own`]'s, or the make's.
    pub fn project(self, cwd: &Path) -> Result<OwnDir, OwnError> {
        self.own(XdgKind::State)?.make_dir(&project_slug(cwd))
    }

    /// A new run of the project at `cwd`, `<project>/<YYYY-MM-DD-HHMMSS>-<pid>/`
    /// — the pid keeps two runs launched in the same second and project
    /// apart, and no other live exarch can choose the name.  Holds
    /// `sessions/<id>/record.jsonl` durably, unlike the disposable [`Scratch`].
    ///
    /// What sits *beside* `sessions/` is the front-end's, not this function's,
    /// and the two front-ends differ because their engines do.  The TUI's
    /// engine is this process, so `TerminalGuard::enter` redirects file
    /// descriptor 2 into `stderr.log` here and everything the engine says
    /// outside the protocol lands in it.  synod's engine is inside a virtual
    /// machine and has no descriptor this process could redirect, so it writes
    /// `engine.log` instead — the guest's own console, fetched from the
    /// machine layer when a conversation fails.  Neither file is promised: a
    /// run that ends well may leave only its records.
    ///
    /// # Errors
    /// [`Self::project`]'s, or the make's.
    pub fn new_run(self, cwd: &Path) -> Result<OwnDir, OwnError> {
        let stamp = format!("{}-{}", stamp_from_secs(now_secs()), std::process::id());
        self.project(cwd)?.make_dir(&stamp)
    }
}

/// The names of the runs of `project` that can be resumed, from newest to
/// oldest: a run directory whose first session recorded something, named for
/// its start.
///
/// Names, not directories: each one held is a descriptor, and a project
/// accumulates runs without limit.
///
/// # Errors
/// The listing of `project`.
pub fn resume_candidates(project: &OwnDir) -> Result<Vec<String>, OwnError> {
    let mut candidates = project
        .list()?
        .into_iter()
        .filter_map(|Entry { name, stat }| {
            if stat.kind != Kind::Dir {
                return None;
            }
            let first = project
                .dir(&name)
                .and_then(|run| run.dir("sessions"))
                .and_then(|sessions| sessions.dir("0"))
                .ok()?;
            let has_record = first
                .stat(RECORD)
                .is_some_and(|record| record.kind == Kind::File);
            let (stamp, _pid) = name.rsplit_once('-')?;
            let timestamp = jiff::civil::DateTime::strptime("%Y-%m-%d-%H%M%S", stamp).ok()?;
            has_record.then_some((timestamp, stat.mtime, name))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| match (a.1, b.1) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => Ordering::Equal,
        })
    });
    Ok(candidates
        .into_iter()
        .rev()
        .map(|(_, _, name)| name)
        .collect())
}

/// The current time in whole unix seconds, 0 if the clock predates the epoch.
/// One spelling for the run-dir stamp, `provider::models`'s cache freshness
/// check, and `provider::oauth`'s token expiry.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}

/// Normalize a run directory or one of its session directories to the run
/// directory.
///
/// # Errors
/// Returns a sentence when the target names a malformed session directory.
pub fn normalize_resume_target(target: &Path) -> Result<PathBuf, String> {
    let Some(session) = target
        .file_name()
        .and_then(|name| name.to_os_string().into_string().ok())
    else {
        return Err(format!(
            "cannot resume {}: the target has no session or run directory name",
            ral_core::path::shown(target)
        ));
    };
    let Some(sessions) = spelled_parent(target).and_then(Path::file_name) else {
        return Ok(target.to_path_buf());
    };
    if sessions != "sessions" {
        return Ok(target.to_path_buf());
    }
    if session.parse::<u64>().is_err() {
        return Err(format!(
            "cannot resume {}: the session directory name must be a number",
            ral_core::path::shown(target)
        ));
    }
    spelled_parent(target)
        .and_then(spelled_parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "cannot resume {}: the session directory has no run-directory parent",
                ral_core::path::shown(target)
            )
        })
}

/// Format a unix timestamp as `YYYY-MM-DD-HHMMSS` (UTC), or the raw seconds if
/// it falls outside the representable range.
fn stamp_from_secs(secs: u64) -> String {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "unix seconds fit i64; from_second still guards its own range"
    )]
    let secs_i64 = secs as i64;
    jiff::Timestamp::from_second(secs_i64).map_or_else(
        |_| secs.to_string(),
        |t| t.strftime("%Y-%m-%d-%H%M%S").to_string(),
    )
}

/// Seed `name` into `shell` twice over: an environment variable, so spawned
/// children inherit it, and a scope binding, so `$name` resolves in ral source.
pub(crate) fn seed_var(shell: &mut Shell, name: &str, value: &str) {
    shell.set_env_var(name, value);
    shell.set_var(name.into(), ral_core::shell::runtime::Value::string(value));
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{
        CONFINED_TOOL_SETTINGS, EXARCH, LOCK_SUFFIX, RECORD, SCRATCH_VAR, Scratch, TOOL_HOMES,
        fresh_lock, guest_scratch_env, normalize_resume_target, reap_unheld, resume_candidates,
        scratch_env,
    };
    use ral_core::own::OwnDir;
    use ral_core::path::GuestPath;
    use std::fs::TryLockError;

    #[test]
    fn scratch_env_names_the_var_the_homes_and_the_settings() {
        let dir = std::path::Path::new("/tmp/x");
        let env = scratch_env(dir).expect("a Unicode path");
        assert_eq!(
            env.len(),
            1 + TOOL_HOMES.len() + CONFINED_TOOL_SETTINGS.len()
        );
        // Every value is native, as the child reads it back: on Windows the
        // subdirectories join with `\`.
        let native = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(env[0], (SCRATCH_VAR.to_owned(), native(dir)));
        for (var, _) in TOOL_HOMES {
            assert!(env.iter().any(|(name, _)| name == var), "{var} is missing");
        }
        for (var, value) in CONFINED_TOOL_SETTINGS {
            assert!(env.contains(&((*var).to_string(), (*value).to_string())));
        }
        let get = |name: &str| {
            env.iter()
                .find(|(var, _)| var == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("XDG_CACHE_HOME"), Some(native(&dir.join("xdg-cache"))));
        assert_eq!(get("TMPDIR"), Some(native(&dir.join("tmp"))));
        assert_eq!(
            get("GOMODCACHE"),
            Some(native(&dir.join("go").join("pkg").join("mod")))
        );
        assert_eq!(
            get("MAVEN_ARGS"),
            Some(format!("-Dmaven.repo.local={}", native(&dir.join("maven"))))
        );
        assert_eq!(get("RUSTUP_HOME"), None);
    }

    /// A guest's scratch is a Posix path on every host: its homes join with
    /// `/` even on Windows, where a native join would name `/tmp\xdg-cache`.
    #[test]
    fn guest_scratch_env_joins_with_slash_on_every_host() {
        let env = guest_scratch_env(&GuestPath::from_static("/tmp"));
        assert_eq!(
            env.len(),
            1 + TOOL_HOMES.len() + CONFINED_TOOL_SETTINGS.len()
        );
        let get = |name: &str| {
            env.iter()
                .find(|(var, _)| var == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get(SCRATCH_VAR), Some("/tmp"));
        assert_eq!(get("TMPDIR"), Some("/tmp/tmp"));
        assert_eq!(get("XDG_CACHE_HOME"), Some("/tmp/xdg-cache"));
        assert_eq!(get("GOMODCACHE"), Some("/tmp/go/pkg/mod"));
        assert_eq!(get("MAVEN_ARGS"), Some("-Dmaven.repo.local=/tmp/maven"));
    }

    #[test]
    fn an_inherited_maven_args_is_replaced() {
        let scratch = Scratch::for_test(EXARCH, "maven-args").expect("test scratch");
        let mut shell = crate::boot::test_shell();
        shell.set_env_var("MAVEN_ARGS", "-Dmaven.repo.local=/home/u/.m2 -T4");
        scratch.install_into(&mut shell);
        assert_eq!(
            shell.env_var("MAVEN_ARGS"),
            Some(format!(
                "-Dmaven.repo.local={}",
                ral_core::path::join_slash(scratch.path(), "maven").to_string_lossy()
            ))
        );
    }

    #[test]
    fn dropping_a_session_scratch_removes_its_directory_and_its_lock() {
        let root = OwnDir::for_test("scratch-drop").expect("a scratch root");
        let scratch = Scratch::in_root(&root).expect("session scratch");
        let name = scratch
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a Unicode name")
            .to_owned();
        let lock = format!("{name}{LOCK_SUFFIX}");
        assert!(root.stat(&name).is_some() && root.stat(&lock).is_some());
        drop(scratch);
        assert!(root.stat(&name).is_none() && root.stat(&lock).is_none());
    }

    /// A lock made fresh is held at once: no sweep can find it unheld.
    #[test]
    fn a_fresh_lock_is_held() {
        let root = OwnDir::for_test("fresh-lock").expect("a scratch root");
        let (name, _held) = fresh_lock(&root).expect("a lock");
        assert!(name.ends_with(LOCK_SUFFIX), "{name}");
        let another = root.open(&name).expect("the lock file");
        assert!(matches!(another.try_lock(), Err(TryLockError::WouldBlock)));
    }

    /// Seed a scratch and its lock file, as [`super::Scratch::in_root`]
    /// would; the lock's name.
    fn seed_scratch(root: &OwnDir, name: &str) -> String {
        let lock = format!("{name}{LOCK_SUFFIX}");
        root.open_write(&lock).expect("seed lock file");
        root.make_dir(name).expect("seed scratch dir");
        lock
    }

    /// The lock alone divides the living from the dead, and a directory with
    /// no lock beside it is none of the reaper's business.
    #[test]
    fn a_scratch_survives_exactly_while_its_lock_is_held() {
        let root = OwnDir::for_test("reap").expect("a scratch root");
        let held = seed_scratch(&root, "held");
        let unheld = seed_scratch(&root, "unheld");
        let mine = seed_scratch(&root, "mine");
        root.make_dir("lockless").expect("seed a lockless dir");

        let live_session = root.open_write(&held).expect("open the lock to hold");
        live_session.try_lock().expect("hold the lock");

        reap_unheld(&root, &mine);

        assert!(
            root.stat("held").is_some(),
            "a held lock is a session still running: its scratch must survive"
        );
        assert!(
            root.stat("unheld").is_none() && root.stat(&unheld).is_none(),
            "an unheld lock answers for nobody, so the scratch and the lock both go"
        );
        assert!(
            root.stat("mine").is_some(),
            "this session's own scratch is never its own to reap"
        );
        assert!(
            root.stat("lockless").is_some(),
            "a directory with no lock beside it is not the reaper's business"
        );
    }

    #[test]
    fn resume_candidates_parse_timestamps_and_skip_mirror_only_runs() {
        let project = OwnDir::for_test("candidates").expect("a project directory");
        let run = |name: &str, recorded: bool| {
            let first = (project.make_dir(name))
                .and_then(|run| run.make_dir("sessions"))
                .and_then(|sessions| sessions.make_dir("0"))
                .expect("a session directory");
            if recorded {
                first.append(RECORD).expect("a durable record");
            }
        };
        run("2026-08-13-120001-1", false);
        run("2026-08-13-120000-9", true);
        run("2026-08-13-120002-10", true);
        run("not-a-run", true);

        let candidates = resume_candidates(&project).expect("candidates");
        assert_eq!(candidates, ["2026-08-13-120002-10", "2026-08-13-120000-9"]);
    }

    #[test]
    fn resume_target_normalizes_any_session_and_refuses_a_malformed_one() {
        let run = std::path::Path::new("/tmp/exarch-run");
        for session in ["sessions/0", "sessions/3"] {
            assert_eq!(normalize_resume_target(&run.join(session)).unwrap(), run);
        }
        let error = normalize_resume_target(&run.join("sessions/main")).unwrap_err();
        assert!(error.contains("must be a number"));
    }

    #[test]
    fn run_lock_is_exclusive_until_its_owner_dies() {
        let run = OwnDir::for_test("run-lock").expect("a run directory");
        let first = super::RunLock::try_acquire(&run).expect("first lock");
        let error = super::RunLock::try_acquire(&run)
            .err()
            .expect("second lock");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        super::RunLock::try_acquire(&run).expect("lock after release");
    }
}
