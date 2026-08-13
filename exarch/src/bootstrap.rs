//! Session boot: the shell every exarch seat starts from, the scratch and XDG
//! directories around it, and the time/slug helpers those share.
//!
//! Nothing here runs per exchange — per-session disk state is
//! [`crate::agent::event::AgentLog`].

use crate::agent::cancel;
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::io::TerminalState;
use ral_core::{Shell, diagnostic};
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

/// The one terminal probe both boot sites take: [`boot_shell`] here, and the
/// `TerminalEndpoint` the identity seat attaches with in `agent::seat`.
pub fn probe_terminal() -> TerminalState {
    let (_mode, terminal, _warn) = TerminalState::probe_from_env();
    terminal
}

/// Build a shell ready for an exarch session, and the one site of the signal
/// ceremony: exarch's cancel chain layers over ral's handlers here, so callers
/// seed per-session variables and nothing more.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn boot_shell() -> Shell {
    ral_core::process::clear();
    ral_core::process::install_handlers();
    cancel::install();

    let terminal = probe_terminal();
    diagnostic::set_terminal(&terminal);
    exarch_shell(terminal)
}

/// The dressing both seats share: [`boot_shell`] and [`engine_boot_shell`]
/// differ only in what they wrap around this.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub(crate) fn exarch_shell(terminal: TerminalState) -> Shell {
    let mut shell =
        ral_core::boot::boot_shell(terminal, &shell_eval::PRELUDE, &builtins::host_surface());
    builtins::install_agent_library(&ral_core::types::Mooring::adrift(), &mut shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(&mut shell);
    shell.set_exit_hints(ral_core::exit_hints::ExitHints::from_text(include_str!(
        "../../data/exit-hints.txt"
    )));
    shell
}

/// The wire engine's `EngineInstaller::boot`, run in the engine process at
/// Attach: [`exarch_shell`] plus an engine-local [`Scratch`] and the identity
/// ceremony's ledgers.
///
/// It carries no signal ceremony (a cancel arrives as a `Control` frame) and
/// no terminal probe (that state is conveyed at Attach).
///
/// # Panics
/// Panics if the agent library or the engine-local scratch cannot be set up.
pub fn engine_boot_shell() -> Shell {
    let mut shell = exarch_shell(TerminalState::default());
    Scratch::new(EXARCH)
        .unwrap_or_else(|e| panic!("exarch engine: scratch creation failed: {e}"))
        .install_into(&mut shell);
    arm_session_ledgers(&mut shell);
    shell
}

/// The ledger half of exarch's session policy — one site for both seats,
/// reached from [`engine_boot_shell`] and from the identity seat's ceremony.
pub fn arm_session_ledgers(shell: &mut Shell) {
    shell.arm_binding_lease(ral_core::types::BindingLease {
        idle_calls: shell_eval::BINDING_IDLE_CALLS,
        large_binding_bytes: shell_eval::LARGE_BINDING_BYTES,
    });
    shell.arm_worker_retention(shell_eval::SETTLED_WORKER_RETENTION);
}

/// Suppress ANSI colour at the source.  A tool call's stdout is a pipe, never a
/// TTY, so these only have to beat config that *forces* colour;
/// [`crate::agent::digest`]'s strip covers the tools that honour neither.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    shell.set_env_var("NO_COLOR", "1");
    shell.set_env_var("CLICOLOR_FORCE", "0");
}

/// Per-session scratch directory, named to the agent under its own [`App`] —
/// `$EXARCH_SCRATCH`, `$SYNOD_SCRATCH`.
///
/// Everything ephemeral the agent scribbles belongs here, because
/// `reasonable` denies writes to the user's real cache dirs.
///
/// A scratch holds whole tool caches — see [`LEGACY_TOOL_HOMES`] — and the
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
/// A session's lock is read by the next session's [`reap_unheld`]; a test's
/// guard is read by nobody, because the test's own end deletes the directory.
#[expect(
    dead_code,
    reason = "each variant is held for what its Drop does — closing the lock's fd, deleting the test's directory — and so is never read"
)]
enum Hold {
    Session(fd_lock::RwLock<File>),
    Test(tempfile::TempDir),
}

/// The lock file's name is its scratch's plus this, so each stands beside the
/// other and either names the other by [`Path::with_extension`].
const LOCK_SUFFIX: &str = ".lock";

/// Build-tool homes that pre-date or ignore XDG, each given its own scratch
/// subdir.  Anything that respects `$XDG_CACHE_HOME` needs no entry here.
const LEGACY_TOOL_HOMES: &[(&str, &str)] = &[
    ("CARGO_HOME", "cargo"),
    ("npm_config_cache", "npm-cache"),
    ("GRADLE_USER_HOME", "gradle"),
    ("GOPATH", "go"),
    ("GOMODCACHE", "go/pkg/mod"),
    ("RUSTUP_HOME", "rustup"),
];

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
        reason = "[io-door:silent:scratch-bootstrap] disposable scratch-dir setup; not turn-time data I/O"
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
        let hold = hold_exclusive(file)?;
        let dir = lock.with_extension("");
        fs::create_dir(&dir)?;
        reap_unheld(&temp, &prefix, &lock);
        Ok(Self {
            app,
            dir,
            hold: Hold::Session(hold),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn app(&self) -> App {
        self.app
    }

    /// The environment variable naming this scratch to the agent.  Public so
    /// [`crate::prompt::host_section`] can ask the scratch for its own name
    /// rather than spell one product's into text both products read.
    pub fn var(&self) -> String {
        format!("{}_SCRATCH", self.app.name().to_uppercase())
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
    /// and `check_disk_warn` sums the two: nested, the log would be counted
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

    /// Seed [`Scratch::var`] and the legacy-tool homes into `shell`, overriding
    /// whatever was inherited: a `CARGO_HOME` under `~/.cargo` lands outside
    /// reasonable's write set, so the sandbox is the trust boundary, not the
    /// environment exarch was launched in.
    pub fn install_into(&self, shell: &mut Shell) {
        let scratch = self.dir.to_string_lossy().into_owned();
        seed_var(shell, &self.var(), &scratch);
        for (var, sub) in LEGACY_TOOL_HOMES {
            let value = format!("{scratch}/{sub}");
            seed_var(shell, var, &value);
        }
    }
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
/// process does answer "held" — but only on the platforms that have `flock`,
/// and a scratch must not depend on that to survive its own reaping.
///
/// Failures are silent throughout: a scratch that resists deletion is a
/// nuisance, never a reason to refuse the session it was made for.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:scratch-bootstrap] disposable scratch-dir setup; not turn-time data I/O"
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

/// Exarch's own directories; synod names its own [`App`].
pub const EXARCH: App = App::new("exarch");

impl App {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// This app's directory under an XDG base: `$XDG_<kind>_HOME/<app>/`.  The
    /// one spelling of that convention — [`App::project_dir`] and
    /// `provider::models`'s cache path both build on it.
    #[must_use]
    #[allow(
        clippy::disallowed_methods,
        reason = "host-env: exarch's own config/state directories live under the launching user's XDG bases"
    )]
    pub fn xdg_dir(self, kind: ral_core::path::basedir::XdgKind) -> PathBuf {
        ral_core::path::basedir::resolve_xdg(kind, &ral_core::host::home()).join(self.0)
    }

    /// `$XDG_STATE_HOME/<app>/<project>/`, `<project>` being the slugified
    /// absolute `cwd`.  Holds `provider::state`'s model selection and the
    /// per-run logs, so a project's state is one directory keyed by where it was
    /// launched, never scattered into cwd.
    #[must_use]
    pub fn project_dir(self, cwd: &str) -> PathBuf {
        self.xdg_dir(ral_core::path::basedir::XdgKind::State)
            .join(project_slug(cwd))
    }

    /// The per-run log directory, `<project>/<YYYY-MM-DD-HHMMSS>-<pid>/` — the
    /// pid keeps two runs launched in the same second and project apart.  Holds
    /// `stderr.log` and `sessions/<id>/{events.jsonl,transcript.jsonl,user.log}`,
    /// durably, unlike the disposable [`Scratch`].
    ///
    /// # Errors
    /// Returns `Err` if creating the directory fails.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:log-run-dir] per-run log dir under XDG state; infra, not turn-time data I/O"
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
            let events = path.join("sessions/0/events.jsonl");
            is_dir.then_some((path, events.is_file()))
        })
        .filter(|(_, has_events)| *has_events)
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

/// Normalize a run directory or its session-0 child to the run directory.
///
/// # Errors
/// Returns a sentence when the target names a child or malformed session
/// directory.
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
    let Ok(id) = session.parse::<u64>() else {
        return Err(format!(
            "cannot resume {}: the session directory name must be a number",
            target.display()
        ));
    };
    if id != 0 {
        return Err(format!(
            "cannot resume {}: session {id} is a child log; children are transient by design and only session 0 can be resumed",
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
    shell.set_var(name.into(), ral_core::types::Value::String(value.into()));
}

#[cfg(test)]
mod tests {
    use super::{
        LOCK_SUFFIX, hold_exclusive, normalize_resume_target, project_slug, reap_unheld,
        resume_candidates_in,
    };
    use std::fs;

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
        fs::write(older.join("sessions/0/events.jsonl"), b"durable").unwrap();
        fs::write(newer.join("sessions/0/events.jsonl"), b"durable").unwrap();

        let candidates = resume_candidates_in(&project).expect("candidates");
        assert_eq!(candidates, vec![newer, older]);
        assert!(!candidates.contains(&transient));
    }

    #[test]
    fn resume_target_normalizes_session_zero_and_refuses_children() {
        let run = std::path::Path::new("/tmp/exarch-run");
        assert_eq!(
            normalize_resume_target(&run.join("sessions/0")).unwrap(),
            run
        );
        let error = normalize_resume_target(&run.join("sessions/1")).unwrap_err();
        assert!(error.contains("children are transient by design"));
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
