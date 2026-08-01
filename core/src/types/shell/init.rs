//! Building a `Shell`, and the startup pass that adopts the host process env.

use super::{Context, LocalState, Mobile, SessionState, Shell};
use crate::source::FileId;
use crate::types::{ControlState, Env, GrantStack};
use std::path::PathBuf;

impl Shell {
    /// Build a new interpreter state with the given terminal state.
    ///
    /// Terminal flags are explicit so a caller cannot leave them all-false,
    /// which would show external commands piped I/O in place of the real
    /// terminal.  The session faces no signals — Ctrl-C and SIGTERM pass it by
    /// — until a host claims them with [`Self::face_signals`].
    pub fn new(terminal: crate::io::TerminalState) -> Self {
        let root = crate::process::DurableRoot::new();
        let mut shell = Self {
            mobile: Mobile {
                scope: Env::new(),
                control: ControlState::default(),
                context: Context {
                    grants: GrantStack::root(),
                    ..Context::default()
                },
            },
            io: crate::io::Io {
                terminal,
                ..Default::default()
            },
            session: SessionState {
                anchor: root.worker(),
                root,
                sources: crate::source::SourceDb::default(),
                root_file: FileId::DUMMY,
                exit_hints: crate::exit_hints::ExitHints::default(),
                builtins: crate::types::BuiltinTable::default(),
                library_docs: std::collections::HashMap::new(),
                terminal_lease: crate::process::TerminalLease::mint_at_startup(
                    terminal.startup_foreground,
                ),
                guest_jail: None,
            },
            local: LocalState::default(),
        };
        shell.install_builtins(crate::builtins::CORE_BUILTINS);
        // Language-given names live in the base scope, ahead of the prelude.
        shell
            .mobile
            .scope
            .install_natives(crate::types::builtin::language_constants());
        shell
    }

    /// Declare this session the process's signal-facing one: the re-minted root
    /// folds the ambient shutdown cause, and stamps every foreground frame with
    /// a birth instant to judge the ambient interrupt watermark against.
    ///
    /// Called at boot by whoever owns the process's signals.  A session forked
    /// with [`Self::fork_session`] starts deaf again and stops through
    /// [`Self::cancel_handle`] instead; several facing sessions in one process
    /// are well-defined, each reading the shared watermark against its own
    /// frames.
    pub fn face_signals(&mut self) {
        self.session.root = crate::process::DurableRoot::signal_facing();
        self.session.anchor = self.session.root.worker();
    }

    /// Adopt the host process env at startup, defaulting anything unset, and
    /// snapshot the process cwd onto the shell-owned
    /// [`Cwd`](crate::types::Cwd) so later reads never resyscall.
    ///
    /// Called once by every front end, so ral code — which reads these as
    /// `$ENV[KEY]` — sees one baseline whoever launched the process.  `SHLVL`
    /// is incremented rather than passed through, as in every other shell.
    /// `PWD` / `OLDPWD` are not seeded here: they are `cwd.current` /
    /// `cwd.previous`, which `apply_env` in
    /// `core/src/runtime/command/process.rs` threads into each child.
    #[allow(
        clippy::disallowed_methods,
        reason = "host-env: seeding the baseline $ENV at boot — the host process env is the source the overlay later shadows"
    )]
    pub fn seed_default_env_vars(&mut self) {
        let home = crate::host::home_or_dot();
        let user = crate::host::user();
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

        // Only when unseeded: a front end whose working directory is not the
        // process cwd states it first through `Shell::seed_cwd`.
        if self.mobile.context.cwd.current.is_none() {
            self.mobile.context.cwd.current = crate::path::process_cwd();
        }
        if self.mobile.context.cwd.previous.is_none() {
            // The launching shell already resolved it; adopt verbatim.
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

        // Compile-time facts, in `$ENV` so rc can branch on the machine
        // without shelling out to `uname`.
        for (k, v) in [
            ("OS_NAME", crate::host::os_name()),
            ("OS_ARCH", crate::host::arch()),
            ("OS_FAMILY", crate::host::family()),
        ] {
            self.mobile.context.set_env_var(k, v);
        }
    }
}
