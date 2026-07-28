//! Session boot: the shell every exarch seat starts from, the scratch and XDG
//! directories around it, and the time/slug helpers those share.  Nothing here
//! runs per exchange — per-session disk state is [`crate::agent::event::AgentLog`].

use crate::agent::cancel;
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::io::TerminalState;
use ral_core::{Shell, diagnostic};
use std::fs;
use std::io;
use std::path::PathBuf;

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
/// ceremony's ledgers, but no signal ceremony (a cancel arrives as a `Control`
/// frame) and no terminal probe (that state is conveyed at Attach).
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
/// `$EXARCH_SCRATCH`, `$SYNOD_SCRATCH`.  Everything ephemeral the agent
/// scribbles belongs here, because `reasonable` denies writes to the user's real
/// cache dirs.  Left on disk at session end: this is OS-managed temp space.
pub struct Scratch {
    app: App,
    dir: PathBuf,
}

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
    /// Create the process's disposable scratch, wiping a stale dir an earlier
    /// run with the same pid left behind.
    ///
    /// # Errors
    /// Returns `Err` if the removal or the creation fails.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:scratch-bootstrap] disposable scratch-dir setup; not turn-time data I/O"
    )]
    pub fn new(app: App) -> io::Result<Self> {
        let dir =
            std::env::temp_dir().join(format!("{}-scratch-{}", app.name(), std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { app, dir })
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

    /// A uniquely-named scratch for tests: [`Self::new`] keys on pid alone, so
    /// concurrent tests would contend on one path.  Public and hidden because
    /// synod's tests need the same scaffolding.
    ///
    /// # Errors
    /// Returns `Err` if the removal or the creation fails.
    #[doc(hidden)]
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:test] test fs scaffolding — a per-test scratch dir"
    )]
    pub fn for_test(app: App, tag: &str) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "{}-scratch-test-{}-{tag}",
            app.name(),
            std::process::id()
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self { app, dir })
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

/// Which product's directories these are.  Exarch and synod are two
/// applications over one engine, and neither may reach into the other's logs or
/// persisted model selection; the name is the only thing that varies, so it is
/// the whole type.
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
    pub fn xdg_dir(self, kind: ral_core::path::basedir::XdgKind) -> PathBuf {
        ral_core::path::basedir::resolve_xdg(kind, &ral_core::path::home_from_env()).join(self.0)
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
    /// `stderr.log` and `sessions/<id>/{events.json,transcript.jsonl,user.log}`,
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
}

/// The current time in whole unix seconds, 0 if the clock predates the epoch.
/// One spelling for the run-dir stamp, `provider::models`'s cache freshness
/// check, and `provider::oauth`'s token expiry.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    use super::project_slug;

    #[test]
    fn slug_joins_path_components_with_dashes() {
        assert_eq!(project_slug("/Users/x/ral-private"), "-Users-x-ral-private");
        assert_eq!(project_slug("/"), "-");
    }
}
