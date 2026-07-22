//! Session boot: shell construction, environment and scratch seeding, the
//! run-log and XDG directory layout, plus the shared time/slug helpers.
//!
//! One-shot bootstrap pieces that live for the lifetime of the process
//! (or, for [`Scratch`], for the lifetime of one root session).
//! Nothing here participates in the per-turn loop.  Per-session disk
//! state (the canonical event log) lives in [`crate::agent::event::AgentLog`].

use crate::agent::cancel;
use crate::shell_eval;
use crate::shell_eval::builtins;
use ral_core::io::TerminalState;
use ral_core::{Shell, diagnostic};
use std::fs;
use std::io;
use std::path::PathBuf;

/// Probe the terminal in the same mode `boot_shell` and every agent's
/// transport endpoint (`Agent::assemble`) both want — honours
/// `RAL_INTERACTIVE_MODE` if set.
pub fn probe_terminal() -> TerminalState {
    let (_mode, terminal, _warn) = TerminalState::probe_from_env();
    terminal
}

/// Build a shell ready for an exarch session.
///
/// This is the one constructor that may boot a session shell: it resets
/// ral's signal-escalation ladder before embedded library evaluation,
/// installs ral's handlers, then immediately layers exarch's cancel chain
/// over them.  Callers should seed per-session variables, not repeat
/// signal ceremony.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
pub fn boot_shell() -> Shell {
    ral_core::process::clear();
    ral_core::process::install_handlers();
    cancel::install();

    let terminal = probe_terminal();
    diagnostic::set_terminal(&terminal);
    dressed_shell(terminal)
}

/// The shell dressing shared by every exarch boot: prelude + host surface,
/// the embedded agent library, colour seeding, exit hints.
///
/// # Panics
/// Panics if the embedded agent library fails to load.
fn dressed_shell(terminal: TerminalState) -> Shell {
    let mut shell =
        ral_core::driver::boot_shell(terminal, &shell_eval::PRELUDE, &builtins::host_surface());
    builtins::install_agent_library(&mut shell)
        .unwrap_or_else(|e| panic!("exarch: embedded agent library failed to load: {e:?}"));
    seed_no_color(&mut shell);
    shell.set_exit_hints(ral_core::exit_hints::ExitHints::from_text(include_str!(
        "../../data/exit-hints.txt"
    )));
    shell
}

/// The wire engine's boot recipe (`EngineInstaller::boot`).
///
/// Run engine-side at Attach — including the fresh process `/clear` boots
/// after killing the old one — the full identity-seat parity:
/// [`dressed_shell`] plus an engine-local [`Scratch`] and the same ledger
/// arming the identity ceremony performs. No signal ceremony (a cancel
/// reaches the engine as a `Control` frame, not a signal) and no terminal
/// probe (the engine has no terminal; its state is conveyed at Attach).
///
/// # Panics
/// Panics if the agent library or the engine-local scratch cannot be set
/// up — a shell missing either would fail mysteriously later.
pub fn engine_boot_shell() -> Shell {
    let mut shell = dressed_shell(TerminalState::default());
    Scratch::new(EXARCH)
        .unwrap_or_else(|e| panic!("exarch engine: scratch creation failed: {e}"))
        .install_into(&mut shell);
    arm_session_ledgers(&mut shell);
    shell
}

/// The ledger half of exarch's session policy.
///
/// Applied by [`engine_boot_shell`] and by the identity seat's own
/// ceremony — one policy site for both seats.
pub fn arm_session_ledgers(shell: &mut Shell) {
    shell.arm_binding_lease(ral_core::types::BindingLease {
        idle_calls: shell_eval::BINDING_IDLE_CALLS,
        large_binding_bytes: shell_eval::LARGE_BINDING_BYTES,
    });
    shell.arm_worker_retention(shell_eval::SETTLED_WORKER_RETENTION);
}

/// Suppress ANSI colour in spawned commands at the source.  Every tool
/// call runs with stdout/stderr captured on a pipe — never a TTY — so
/// conforming tools already emit no colour; these override user config
/// and an inherited `CLICOLOR_FORCE` that *force* it.  That keeps a
/// captured value byte-identical to the text [`crate::agent::digest`] shows
/// the model; its ANSI strip remains the boundary guarantee for tools
/// that ignore the convention.  Env-only, no ral scope binding: the
/// agent has no reason to read these back.
pub(crate) fn seed_no_color(shell: &mut Shell) {
    shell.set_env_var("NO_COLOR", "1");
    shell.set_env_var("CLICOLOR_FORCE", "0");
}

/// Per-session scratch directory, exposed to the agent under its own
/// [`App`]'s name — `$EXARCH_SCRATCH`, `$SYNOD_SCRATCH`.
///
/// Caches the agent might want to scribble to (build artefacts, package
/// manager state, anything ephemeral) live here instead of in the user's
/// real cache dirs (`~/.cargo/registry`, `~/.npm`, …) — those are denied
/// for write by `reasonable`, so a direct write there fails loudly.
///
/// To make this transparent to the agent, [`Scratch::install_into`]
/// redirects a small fixed list of legacy tool env vars (`CARGO_HOME`,
/// `npm_config_cache`, `GRADLE_USER_HOME`, `GOPATH`, `GOMODCACHE`,
/// `RUSTUP_HOME`) to subdirs of the scratch.  Modern tools that
/// respect `$XDG_CACHE_HOME` need no per-tool redirection — the
/// `xdg:cache` write admit in reasonable handles them.
///
/// Intentionally left on disk when the session ends; scratch lives
/// under OS-managed temp space and can be swept by the platform.
pub struct Scratch {
    app: App,
    dir: PathBuf,
}

/// Legacy build-tool home env vars that pre-date or ignore XDG.  Each
/// gets a dedicated subdir under the session scratch so the toolchains
/// can do their own bookkeeping without colliding.  This list is
/// deliberately stable and short — modern tools added since ~2018
/// (uv, pnpm, bun, mise, hatch, ruff, …) respect `$XDG_CACHE_HOME`
/// and don't need an entry here.
const LEGACY_TOOL_HOMES: &[(&str, &str)] = &[
    ("CARGO_HOME", "cargo"),
    ("npm_config_cache", "npm-cache"),
    ("GRADLE_USER_HOME", "gradle"),
    ("GOPATH", "go"),
    ("GOMODCACHE", "go/pkg/mod"),
    ("RUSTUP_HOME", "rustup"),
];

impl Scratch {
    /// Create the process's disposable scratch directory, wiping any stale
    /// dir left by a prior run with the same pid.
    ///
    /// # Errors
    /// Returns `Err` if removing a stale scratch directory or creating the
    /// fresh one fails.
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

    /// Which product this scratch belongs to.
    pub fn app(&self) -> App {
        self.app
    }

    /// The environment variable naming this scratch to the agent —
    /// `EXARCH_SCRATCH` under exarch, `SYNOD_SCRATCH` under synod.
    ///
    /// Public because the prompt must name the same variable the shell was
    /// seeded with: [`prompt::host_section`](crate::prompt::host_section)
    /// asks the scratch for its own name rather than spelling one product's
    /// into text both products read.
    pub fn var(&self) -> String {
        format!("{}_SCRATCH", self.app.name().to_uppercase())
    }

    /// A uniquely-named scratch for tests: [`Self::new`]'s dir is keyed by
    /// pid alone, so two tests constructing scratches concurrently would
    /// contend on one path — the `tag` keeps each test's dir its own.
    ///
    /// Public, and hidden from the docs, because synod's tests need the
    /// same scaffolding: a second copy of it in a second crate would be a
    /// second thing to keep true.  A live run always takes [`Self::new`].
    ///
    /// # Errors
    /// Returns `Err` if removing a stale directory or creating the fresh
    /// one fails.
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

    /// Seed the scratch variable ([`Scratch::var`]) and the
    /// legacy-tool env vars into
    /// `shell` — both the env-var map (so child processes inherit
    /// them) and the ral-side bindings (so the same names resolve
    /// inside ral source).  Always overrides: a user pre-set
    /// `CARGO_HOME` pointing into `~/.cargo` would land outside
    /// reasonable's write set and fail mysteriously, so the sandbox
    /// is the trust boundary, not the inherited environment.
    pub fn install_into(&self, shell: &mut Shell) {
        let scratch = self.dir.to_string_lossy().into_owned();
        seed_var(shell, &self.var(), &scratch);
        for (var, sub) in LEGACY_TOOL_HOMES {
            let value = format!("{scratch}/{sub}");
            seed_var(shell, var, &value);
        }
    }
}

/// Which product's directories these are.
///
/// Exarch and synod are two applications over one engine, and each owns
/// its own XDG subtree and its own scratch: a synod session must never
/// write into an exarch run's logs or read its persisted model
/// selection.  The name is the only thing that varies, so it is the
/// whole type — every directory below is `<app>` plus a convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct App(&'static str);

/// Exarch's own directories.  Synod names its own [`App`].
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

    /// This app's directory under an XDG base: `$XDG_<kind>_HOME/<app>/`.
    ///
    /// The one spelling of the app-subdir convention — [`App::project_dir`]
    /// (state home) and the model cache (`models::cache_path`, cache home)
    /// both build on it.
    #[must_use]
    pub fn xdg_dir(self, kind: ral_core::path::basedir::XdgKind) -> PathBuf {
        ral_core::path::basedir::resolve_xdg(kind, &ral_core::path::home_from_env()).join(self.0)
    }

    /// The per-project directory `$XDG_STATE_HOME/<app>/<project>/`, where
    /// `<project>` is the slugified absolute `cwd` (see [`project_slug`]).
    ///
    /// Both the persisted model selection (`state.json`) and the per-run
    /// session logs live under it, so a project's state is one findable
    /// directory keyed by where it was launched — never scattered into cwd.
    #[must_use]
    pub fn project_dir(self, cwd: &str) -> PathBuf {
        self.xdg_dir(ral_core::path::basedir::XdgKind::State)
            .join(project_slug(cwd))
    }

    /// The per-run log directory:
    /// `$XDG_STATE_HOME/<app>/<project>/<run>/`, where `<run>` is
    /// `<YYYY-MM-DD-HHMMSS>-<pid>`, unique per launch so successive runs in
    /// the same project never overwrite one another.  Holds `stderr.log`
    /// and `sessions/<id>/{events.json,transcript.jsonl,user.log}`.  Unlike
    /// the disposable [`Scratch`] this is durable state under the user's
    /// XDG state home, so it survives an abnormal exit and stays findable
    /// without a symlink.
    ///
    /// # Errors
    /// Returns `Err` if creating the per-run log directory fails.
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

/// The current time in whole unix seconds, or 0 if the clock is before the
/// epoch. The one spelling shared by the run-dir stamp, the model cache's
/// freshness check ([`crate::provider::models`]), and the OAuth token expiry
/// ([`crate::provider::oauth`]).
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Format a Unix timestamp as `YYYY-MM-DD-HHMMSS` (UTC) via `jiff`.
/// Falls back to the raw seconds string if the timestamp is out of range.
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

/// Slugify an absolute path into one directory-name component by joining
/// its components with `-`, a leading `-` standing in for the root:
/// `/Users/x/proj` becomes `-Users-x-proj`.  Built on
/// [`std::path::Path::components`] so it is correct across `/`, `\`, and
/// Windows drive prefixes without a slugifier dependency.
// `Path::components` is the correct cross-platform decomposition for a
// slug; this is not a path-resolution site, so it is exempt from the
// crate::path discipline.
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

/// Seed `name` into `shell` as both an environment variable (so spawned
/// child processes inherit it) and a ral-side scope binding (so `$name`
/// resolves in ral source).  Shared by [`Scratch::install_into`] and the
/// per-agent `EXARCH_SESSION_DIR` seeding in [`crate::agent`].
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
