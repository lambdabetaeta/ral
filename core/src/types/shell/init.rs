//! Shell construction and the startup env-var seeding pass.
//!
//! [`Shell::new`] builds an empty interpreter state — root grant frame,
//! defaulted env, no audit trail.  [`Shell::seed_default_env_vars`] is
//! the front-end startup hook that adopts the host process env
//! (`HOME`, `USER`, `PATH`, `SHELL`, `TERM`, `LANG`, `LOGNAME`,
//! multiplexer / terminal passthroughs) into the dynamic context,
//! increments `SHLVL`, and snapshots the
//! process cwd into the shell-owned [`Cwd`](crate::types::Cwd) pair so
//! later reads consult shell state instead of resyscalling.

use super::{Context, LocalState, Mobile, SessionState, Shell, TurnState};
use crate::types::{ControlState, Env, GrantStack, LocationCursor};
use std::path::PathBuf;

impl Shell {
    /// Build a new interpreter state with the given terminal state.
    ///
    /// The terminal flags are passed explicitly so callers cannot
    /// accidentally leave them at all-false — which would cause
    /// external commands to see piped I/O instead of the real
    /// terminal.
    ///
    /// Installs [`crate::builtins::CORE_BUILTINS`] into this shell so
    /// the language's built-in surface is reachable from the first
    /// command.
    pub fn new(terminal: crate::io::TerminalState) -> Self {
        crate::builtins::ensure_core_builtins_registered();
        let root = crate::process::DurableRoot::new();
        let mut shell = Shell {
            mobile: Mobile {
                scope: Env::new(),
                control: ControlState::default(),
                context: Context {
                    grants: GrantStack::root(),
                    ..Context::default()
                },
            },
            turn: TurnState {
                io: crate::io::Io {
                    terminal,
                    ..Default::default()
                },
                surface: None,
                cancel: root.child(),
                loc: LocationCursor::default(),
                detached_ceiling: None,
                // The boot frame holds no terminal authority; a host states it
                // per turn via `TurnRequest::terminal`. `Denied` is the safe
                // default so a frame with no stated policy never foregrounds.
                terminal_access: crate::types::TerminalAccess::Denied,
            },
            session: SessionState {
                root,
                sources: crate::diagnostic::SourceDb::default(),
                exit_hints: crate::exit_hints::ExitHints::default(),
                builtins: crate::types::BuiltinTable::default(),
                // Mint the session's lease from the same predicate that
                // populates `startup_foreground`. `None` off Unix and whenever
                // ral did not own the terminal foreground at startup.
                terminal_lease: crate::process::TerminalLease::mint_at_startup(
                    terminal.startup_foreground,
                ),
            },
            local: LocalState::default(),
        };
        shell.install_builtins(crate::builtins::CORE_BUILTINS);
        shell
    }

    /// Adopt the host process env at startup.
    ///
    /// Seeds the well-known variables (`HOME`, `USER`, `PATH`,
    /// `SHELL`, `TERM`, `LANG`, `LOGNAME`, `SHLVL`, multiplexer and
    /// terminal passthroughs) into `context.env_overrides`, filling in
    /// sensible defaults for anything unset.  These are read from ral
    /// code as `$env[KEY]`; the environment is dynamic state, not a
    /// lexical binding.  Called once at startup by every front end —
    /// interactive `ral`, `exarch`, batch scripts — so the language
    /// sees a consistent baseline regardless of who launched the
    /// process.
    ///
    /// Also seeds the shell-owned [`Cwd`](crate::types::Cwd) pair on
    /// `context.cwd` from the process cwd at startup, so subsequent
    /// reads consult the logical field rather than re-syscalling.
    /// `PWD` / `OLDPWD` live on `cwd.current` / `cwd.previous`;
    /// [`crate::runtime::command::process::apply_env`] threads them
    /// into spawned child commands.  `SHLVL` is always incremented
    /// rather than passed through, matching every other shell.
    pub fn seed_default_env_vars(&mut self) {
        let home = crate::path::home_from_env_or_dot();
        let user = crate::path::user_name_from_env();
        let path = std::env::var("PATH").unwrap_or_else(|_| {
            if cfg!(windows) {
                "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem".into()
            } else {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
            }
        });
        let shell_path = std::env::var("SHELL").unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.into_os_string().into_string().ok())
                .unwrap_or_else(|| "ral".into())
        });
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
        let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
        let logname = std::env::var("LOGNAME").unwrap_or_else(|_| user.clone());

        // Adopt the process cwd as the persistent logical cwd, so
        // subsequent path resolution and child-process cwd inheritance
        // flow through shell state instead of resyscalling.  `OLDPWD`,
        // if the launching shell set it, becomes the logical companion;
        // otherwise leave it `None` (no prior cwd to return to via
        // `cd -`).
        if self.mobile.context.cwd.current.is_none() {
            self.mobile.context.cwd.current = crate::path::process_cwd();
        }
        if self.mobile.context.cwd.previous.is_none() {
            // Env-var lift: OLDPWD is whatever the launching shell set
            // it to, already-resolved; we adopt it verbatim.
            #[allow(clippy::disallowed_methods)]
            let oldpwd = std::env::var_os("OLDPWD").map(PathBuf::from);
            self.mobile.context.cwd.previous = oldpwd;
        }

        let context = &mut self.mobile.context;
        let mut install = |k: &str, v: String| {
            context.set_env_var_or_keep(k, v);
        };
        for (k, v) in [
            ("HOME", home),
            ("USER", user),
            ("PATH", path),
            ("SHELL", shell_path),
            ("TERM", term),
            ("LANG", lang),
            ("LOGNAME", logname),
        ] {
            install(k, v);
        }
        for k in [
            "TMUX",
            "TMUX_PANE",
            "STY",
            "COLORTERM",
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
        ] {
            if let Ok(v) = std::env::var(k) {
                install(k, v);
            }
        }
        let shlvl = std::env::var("SHLVL")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0)
            .saturating_add(1)
            .to_string();
        self.mobile.context.set_env_var("SHLVL", shlvl);

        // Machine facts (compile-time constants): exposed via `$env` so rc
        // can branch on the OS/arch without shelling out to `uname`.
        for (k, v) in [
            ("OS_NAME", crate::host::os_name()),
            ("OS_ARCH", crate::host::arch()),
            ("OS_FAMILY", crate::host::family()),
        ] {
            self.mobile.context.set_env_var(k, v);
        }
    }
}
