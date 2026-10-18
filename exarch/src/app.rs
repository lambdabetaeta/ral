//! The apps and their scratch and XDG directories, and the time/slug helpers
//! those share.

use ral_core::Shell;
use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RunLock {
    _lock: fd_lock::RwLock<File>,
}

impl RunLock {
    /// Acquire the run's advisory write lock without waiting.
    ///
    /// # Errors
    /// Returns an error when another exarch owns the lock.
    pub fn try_acquire(run_dir: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(run_dir.join("run.lock"))?;
        Ok(Self {
            _lock: hold_exclusive(file)?,
        })
    }
}

/// Take `file`'s exclusive advisory lock and keep it for as long as this
/// process lives.
///
/// The lock belongs to the open file description, which the returned value
/// holds open, so forgetting the guard forgoes fd-lock's unlock-on-drop and
/// nothing else.  The OS releases the lock when the process ends, by whatever
/// means it ends — which is what makes the lock an honest answer to "is
/// anyone still using this?".
fn hold_exclusive(file: File) -> io::Result<fd_lock::RwLock<File>> {
    let mut lock = fd_lock::RwLock::new(file);
    std::mem::forget(lock.try_write()?);
    Ok(lock)
}

/// Per-session scratch directory, named to the agent under its own [`App`] —
/// `$EXARCH_SCRATCH`, `$SYNOD_SCRATCH`.
///
/// Everything ephemeral the agent scribbles belongs here, because
/// `reasonable` denies writes to the user's real cache dirs.
///
/// A scratch holds whole tool caches — see [`TOOL_HOMES`] — and the
/// temp directory around it is swept by the OS on no schedule worth trusting:
/// Windows never sweeps it, macOS only across a reboot, and Linux only where
/// `/tmp` is not a tmpfs.  So exarch sweeps its own: a session holds a lock
/// beside its scratch for as long as it runs, and [`Scratch::new`] deletes
/// the scratches no lock answers for.
pub struct Scratch {
    app: App,
    dir: PathBuf,
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
    Test(tempfile::TempDir),
}

/// A live session's scratch and the lock that says so.
struct SessionHold {
    dir: PathBuf,
    lock_path: PathBuf,
    lock: Option<fd_lock::RwLock<File>>,
}

impl Drop for SessionHold {
    /// The directory, then the lock: closed before it is unlinked, because
    /// Windows will not unlink an open file.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        drop(self.lock.take());
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// The lock file's name is its scratch's plus this, so each stands beside the
/// other and either names the other by [`Path::with_extension`].
const LOCK_SUFFIX: &str = ".lock";

/// What a tool home takes under a scratch: a subdirectory, or a command-line
/// flag whose value is one.
#[derive(Clone, Copy)]
enum Home {
    Dir(&'static str),
    Flag(&'static str, &'static str),
}

impl Home {
    fn value(self, scratch: &str) -> String {
        match self {
            Self::Dir(sub) => format!("{scratch}/{sub}"),
            Self::Flag(prefix, sub) => format!("{prefix}{scratch}/{sub}"),
        }
    }
}

/// Every cache a session writes lives in its scratch: each variable is
/// replaced, whatever the host inherited, by a value under the scratch.
const TOOL_HOMES: &[(&str, Home)] = &[
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
/// reaches securityd — the keychain door no profile admits — and reports the
/// denial as `ssl handshake -9808`, which cargo then blames on a missing
/// revision.  The `git` binary's TLS needs no such door, so cargo fetches
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
    /// Returns `Err` if the lock file or the directory cannot be created.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:scratch-bootstrap] disposable scratch-dir setup; not turn-time data I/O"
    )]
    pub fn new(app: App) -> io::Result<Self> {
        let temp = std::env::temp_dir();
        let prefix = format!("{}-scratch-", app.name());
        let (file, lock) = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(LOCK_SUFFIX)
            .tempfile_in(&temp)?
            .keep()
            .map_err(|kept| kept.error)?;
        let lock_guard = hold_exclusive(file)?;
        let dir = lock.with_extension("");
        fs::create_dir(&dir)?;
        reap_unheld(&temp, &prefix, &lock);
        Ok(Self {
            app,
            hold: Hold::Session(SessionHold {
                dir: dir.clone(),
                lock_path: lock,
                lock: Some(lock_guard),
            }),
            dir,
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.dir
    }

    /// The environment variable naming this scratch to the agent.
    pub fn var(&self) -> String {
        self.app.scratch_var()
    }

    /// A scratch for one test, deleted when the returned value falls.  A test
    /// process ends normally, so the guard suffices and no lock is taken.
    ///
    /// `tag` names the test, and only so that the rare directory outliving a
    /// killed test run says which test left it.
    ///
    /// Deliberately outside [`Self::new`]'s `<app>-scratch-` prefix, so a live
    /// session's [`reap_unheld`] never considers a test's directory.
    ///
    /// Public and hidden because synod's tests need the same scaffolding.
    ///
    /// # Errors
    /// Returns `Err` if the directory cannot be created.
    #[doc(hidden)]
    pub fn for_test(app: App, tag: &str) -> io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix(&format!("{}-test-{tag}-", app.name()))
            .tempdir()?;
        let dir = root.path().join("scratch");
        fs::create_dir(&dir)?;
        Ok(Self {
            app,
            dir,
            hold: Hold::Test(root),
        })
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
    #[doc(hidden)]
    pub fn test_sibling(&self, name: &str) -> io::Result<PathBuf> {
        let Hold::Test(root) = &self.hold else {
            return Err(io::Error::other(
                "only a test scratch has room beside it; a session's scratch stands alone",
            ));
        };
        let path = root.path().join(name);
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    /// Seed [`Scratch::var`], the [`TOOL_HOMES`] and
    /// [`CONFINED_TOOL_SETTINGS`] into `shell`, overriding whatever was
    /// inherited: a `CARGO_HOME` under `~/.cargo` lands outside reasonable's
    /// write set, so the sandbox is the trust boundary, not the environment
    /// exarch was launched in.  One seeder for all three, because the seat's
    /// test double mirrors this call and a second site would drift from it.
    pub fn install_into(&self, shell: &mut Shell) {
        for (name, value) in self.env() {
            seed_var(shell, &name, &value);
        }
    }

    /// What [`Self::install_into`] seeds, as the pairs an `Attach` names.
    pub fn env(&self) -> Vec<(String, String)> {
        scratch_env(self.app, &self.dir)
    }
}

/// What a scratch at `dir` names to the agent.
///
/// The scratch var, the tool homes under it, and the confined tool settings:
/// one spelling for a host's own `Scratch` and for a guest path the host
/// cannot hold.
pub fn scratch_env(app: App, dir: &Path) -> Vec<(String, String)> {
    let scratch = dir.to_string_lossy().into_owned();
    let homes = TOOL_HOMES
        .iter()
        .map(|(var, home)| ((*var).to_string(), home.value(&scratch)));
    let settings = CONFINED_TOOL_SETTINGS
        .iter()
        .map(|(var, value)| ((*var).to_string(), (*value).to_string()));
    std::iter::once((app.scratch_var(), scratch.clone()))
        .chain(homes)
        .chain(settings)
        .collect()
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
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:scratch-reap] sweeps the scratch dirs no live process still holds a lock on; not turn-time data I/O"
)]
fn reap_unheld(temp: &Path, prefix: &str, mine: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let lock = entry.path();
        let Some(name) = lock.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if lock == mine || !name.starts_with(prefix) || !name.ends_with(LOCK_SUFFIX) {
            continue;
        }
        // Opened, never created: a lock file that has just been deleted names
        // a scratch that is already somebody else's business.
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock) else {
            continue;
        };
        let mut unheld = fd_lock::RwLock::new(file);
        if unheld.try_write().is_err() {
            continue;
        }
        // Released and closed before the deletion, because Windows will not
        // unlink a file this process still holds open.  Another session may
        // take it in between and delete the same scratch; one of the two then
        // fails harmlessly, and no *live* session can be the one that takes
        // it, since a new session always makes a new name.
        drop(unheld);
        let _ = fs::remove_dir_all(lock.with_extension(""));
        let _ = fs::remove_file(&lock);
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

    /// The variable naming this app's scratch to the agent.
    #[must_use]
    pub fn scratch_var(self) -> String {
        format!("{}_SCRATCH", self.0.to_uppercase())
    }

    /// This app's directory under an XDG base: `$XDG_<kind>_HOME/<app>/`.  The
    /// one spelling of that convention — [`App::project_dir`] and
    /// `provider::models`'s cache path both build on it.
    ///
    /// With no home and no absolute override the base is the temp dir, so a
    /// run still records and configures — losing that state on reboot, where
    /// defaulting under the cwd would instead scatter it through the working
    /// tree the agent is editing.
    #[must_use]
    #[allow(
        clippy::disallowed_methods,
        reason = "host-env: exarch's own config/state directories live under the launching user's XDG bases"
    )]
    pub fn xdg_dir(self, kind: ral_core::path::XdgKind) -> PathBuf {
        ral_core::host::xdg(kind)
            .unwrap_or_else(std::env::temp_dir)
            .join(self.0)
    }

    /// `$XDG_STATE_HOME/<app>/<project>/`, `<project>` being the slugified
    /// absolute `cwd`.  Holds `provider::state`'s model selection and the
    /// per-run logs, so a project's state is one directory keyed by where it was
    /// launched, never scattered into cwd.
    #[must_use]
    pub fn project_dir(self, cwd: &str) -> PathBuf {
        self.xdg_dir(ral_core::path::XdgKind::State)
            .join(project_slug(cwd))
    }

    /// The per-run log directory, `<project>/<YYYY-MM-DD-HHMMSS>-<pid>/` — the
    /// pid keeps two runs launched in the same second and project apart.  Holds
    /// `sessions/<id>/{record.jsonl,record.log,user.log}` durably, unlike the
    /// disposable [`Scratch`].
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
    /// Returns `Err` if creating the directory fails.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:log-run-dir] per-run log dir under XDG state; infra, not turn-time data I/O"
    )]
    pub fn log_run_dir(self, cwd: &str) -> io::Result<PathBuf> {
        let stamp = format!("{}-{}", stamp_from_secs(now_secs()), std::process::id());
        let dir = self.project_dir(cwd).join(stamp);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// List eligible run directories from newest to oldest.
    ///
    /// # Errors
    /// Returns an error when the project state directory cannot be inspected.
    pub fn resume_candidates(self, cwd: &str) -> io::Result<Vec<PathBuf>> {
        resume_candidates_in(&self.project_dir(cwd))
    }
}

fn resume_candidates_in(project: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(project) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let is_dir = entry.file_type().ok()?.is_dir();
            let record = path.join("sessions/0/record.jsonl");
            is_dir.then_some((path, record.is_file()))
        })
        .filter(|(_, has_record)| *has_record)
        .filter_map(|(path, _)| {
            let name = path.file_name()?.to_str()?;
            let (stamp, _pid) = name.rsplit_once('-')?;
            let timestamp = jiff::civil::DateTime::strptime("%Y-%m-%d-%H%M%S", stamp).ok()?;
            let modified = fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok());
            Some((timestamp, modified, path))
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
        .map(|(_, _, path)| path)
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
    let Some(session) = target.file_name().and_then(|name| name.to_str()) else {
        return Err(format!(
            "cannot resume {}: the target has no session or run directory name",
            target.display()
        ));
    };
    let Some(sessions) = target.parent().and_then(Path::file_name) else {
        return Ok(target.to_path_buf());
    };
    if sessions != "sessions" {
        return Ok(target.to_path_buf());
    }
    if session.parse::<u64>().is_err() {
        return Err(format!(
            "cannot resume {}: the session directory name must be a number",
            target.display()
        ));
    }
    target
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "cannot resume {}: the session directory has no run-directory parent",
                target.display()
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

/// Slugify an absolute path into one directory-name component: `/Users/x/proj`
/// becomes `-Users-x-proj`, the leading `-` standing in for the root.  Built on
/// [`std::path::Path::components`], so `\` and drive prefixes come out right too.
// The `Path` here is only decomposed, never resolved, so this is not one of the
// construction sites `ral_core::path` exists to own.
#[allow(clippy::disallowed_methods)]
fn project_slug(cwd: &str) -> String {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    let mut rooted = false;
    for comp in std::path::Path::new(cwd).components() {
        match comp {
            Component::Prefix(p) => {
                let drive: String = p
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !drive.is_empty() {
                    parts.push(drive);
                }
            }
            Component::RootDir => rooted = true,
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir | Component::ParentDir => {}
        }
    }
    let body = parts.join("-");
    let slug = if rooted { format!("-{body}") } else { body };
    if slug.is_empty() { "_".into() } else { slug }
}

/// Seed `name` into `shell` twice over: an environment variable, so spawned
/// children inherit it, and a scope binding, so `$name` resolves in ral source.
pub(crate) fn seed_var(shell: &mut Shell, name: &str, value: &str) {
    shell.set_env_var(name, value);
    shell.set_var(name.into(), ral_core::types::Value::string(value));
}

#[cfg(test)]
mod tests {
    use super::{
        CONFINED_TOOL_SETTINGS, EXARCH, LOCK_SUFFIX, Scratch, TOOL_HOMES, hold_exclusive,
        normalize_resume_target, project_slug, reap_unheld, resume_candidates_in, scratch_env,
    };
    use std::fs;

    #[test]
    fn scratch_env_names_the_var_the_homes_and_the_settings() {
        let env = scratch_env(EXARCH, std::path::Path::new("/tmp/x"));
        assert_eq!(
            env.len(),
            1 + TOOL_HOMES.len() + CONFINED_TOOL_SETTINGS.len()
        );
        assert_eq!(env[0], (EXARCH.scratch_var(), "/tmp/x".to_string()));
        for (var, home) in TOOL_HOMES {
            assert!(env.contains(&((*var).to_string(), home.value("/tmp/x"))));
        }
        for (var, value) in CONFINED_TOOL_SETTINGS {
            assert!(env.contains(&((*var).to_string(), (*value).to_string())));
        }
        let get = |name: &str| {
            env.iter()
                .find(|(var, _)| var == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("XDG_CACHE_HOME"), Some("/tmp/x/xdg-cache"));
        assert_eq!(get("GOMODCACHE"), Some("/tmp/x/go/pkg/mod"));
        assert_eq!(get("MAVEN_ARGS"), Some("-Dmaven.repo.local=/tmp/x/maven"));
        assert_eq!(get("RUSTUP_HOME"), None);
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
                "-Dmaven.repo.local={}/maven",
                scratch.path().display()
            ))
        );
    }

    #[test]
    fn dropping_a_session_scratch_removes_its_directory_and_its_lock() {
        let scratch = Scratch::new(EXARCH).expect("session scratch");
        let dir = scratch.path().to_path_buf();
        let lock = dir.with_extension(LOCK_SUFFIX.trim_start_matches('.'));
        assert!(dir.exists() && lock.exists());
        drop(scratch);
        assert!(!dir.exists() && !lock.exists());
    }

    /// Seed a scratch and its lock file, as [`super::Scratch::new`] would.
    fn seed_scratch(temp: &std::path::Path, name: &str) -> std::path::PathBuf {
        let lock = temp.join(format!("{name}{LOCK_SUFFIX}"));
        fs::write(&lock, b"").expect("seed lock file");
        fs::create_dir_all(temp.join(name)).expect("seed scratch dir");
        lock
    }

    /// The lock alone divides the living from the dead, and nothing outside
    /// the prefix is any of the reaper's business.
    #[test]
    fn a_scratch_survives_exactly_while_its_lock_is_held() {
        let temp = tempfile::tempdir().expect("temp root");
        let held = seed_scratch(temp.path(), "exarch-scratch-held");
        let unheld = seed_scratch(temp.path(), "exarch-scratch-unheld");
        let mine = seed_scratch(temp.path(), "exarch-scratch-mine");
        let synod = seed_scratch(temp.path(), "synod-scratch-elsewhere");
        let fixture = temp.path().join("exarch-test-some-tag-a7bx");
        fs::create_dir_all(&fixture).expect("seed fixture");

        let file = fs::File::options()
            .read(true)
            .write(true)
            .open(&held)
            .expect("open the lock to hold");
        let _live_session = hold_exclusive(file).expect("hold the lock");

        reap_unheld(temp.path(), "exarch-scratch-", &mine);

        assert!(
            held.with_extension("").exists(),
            "a held lock is a session still running: its scratch must survive"
        );
        assert!(
            !unheld.with_extension("").exists() && !unheld.exists(),
            "an unheld lock answers for nobody, so the scratch and the lock both go"
        );
        assert!(
            mine.with_extension("").exists(),
            "this session's own scratch is never its own to reap"
        );
        assert!(
            synod.with_extension("").exists(),
            "synod's scratches are outside exarch's prefix and untouchable"
        );
        assert!(
            fixture.exists(),
            "a test fixture has no lock and sits outside the prefix: not the reaper's business"
        );
    }

    #[test]
    fn slug_joins_path_components_with_dashes() {
        assert_eq!(project_slug("/Users/x/ral-private"), "-Users-x-ral-private");
        assert_eq!(project_slug("/"), "-");
    }

    #[test]
    fn resume_candidates_parse_timestamps_and_skip_mirror_only_runs() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let transient = project.join("2026-08-13-120001-1");
        let older = project.join("2026-08-13-120000-9");
        let newer = project.join("2026-08-13-120002-10");
        for run in [&transient, &older, &newer] {
            fs::create_dir_all(run.join("sessions/0")).unwrap();
        }
        fs::write(older.join("sessions/0/record.jsonl"), b"durable").unwrap();
        fs::write(newer.join("sessions/0/record.jsonl"), b"durable").unwrap();

        let candidates = resume_candidates_in(&project).expect("candidates");
        assert_eq!(candidates, vec![newer, older]);
        assert!(!candidates.contains(&transient));
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
        let root = tempfile::tempdir().expect("temp root");
        let first = super::RunLock::try_acquire(root.path()).expect("first lock");
        let error = super::RunLock::try_acquire(root.path())
            .err()
            .expect("second lock");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        super::RunLock::try_acquire(root.path()).expect("lock after release");
    }
}
