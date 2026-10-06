//! Building a `Shell`, and the startup pass that adopts the host process env.

use super::{Context, LocalState, SessionState, Shell};
use crate::source::FileId;
use crate::types::{Env, GrantStack, Signature};
use std::sync::Arc;

impl Shell {
    /// Build a new interpreter state with the given terminal state.
    ///
    /// Terminal flags are explicit so a caller cannot leave them all-false,
    /// which would show external commands piped I/O in place of the real
    /// terminal.  The session faces no signals: its host forwards them as
    /// `Control`.
    pub fn new(terminal: crate::io::TerminalState) -> Self {
        let root = crate::process::DurableRoot::new();
        let mut shell = Self {
            env: Env::new(),
            sig: Arc::new(Signature::default()),
            context: Context {
                grants: GrantStack::root(),
                ..Context::default()
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
                stack_limit: super::DEFAULT_STACK_LIMIT,
            },
            local: LocalState::default(),
        };
        shell.install_builtins(crate::builtins::CORE_BUILTINS);
        shell.install_builtins(crate::builtins::BOUNDARY_BUILTINS);
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        // Language-given names live in Σ, ahead of the prelude.
        Arc::make_mut(&mut shell.sig).install_natives(crate::types::builtin::language_constants());
        shell
    }

    /// Adopt the host process env at startup, defaulting anything unset, and
    /// snapshot the process cwd onto the shell-owned
    /// [`Cwd`](crate::types::Cwd) so later reads never resyscall.
    ///
    /// Called once by every front end, so ral code — which reads these as
    /// `!{env}[KEY]` — sees one baseline whoever launched the process.  `SHLVL`
    /// is incremented rather than passed through, as in every other shell.
    /// `PWD` is not seeded here: it is the cwd cell, which `apply_env` in
    /// `core/src/runtime/command/process.rs` threads into each child.
    #[allow(
        clippy::disallowed_methods,
        reason = "host-env: seeding the baseline `env` at boot — the host process env is the source the overlay later shadows"
    )]
    pub(crate) fn seed_default_env_vars(&mut self) {
        let home = crate::host::home();
        let user = crate::host::user();
        let path = inherited_or("PATH", || {
            Some(if cfg!(windows) {
                "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem".into()
            } else {
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
            })
        });
        let shell_path = inherited_or("SHELL", || {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.into_os_string().into_string().ok())
                .or_else(|| Some("ral".into()))
        });
        let term = inherited_or("TERM", || Some("xterm-256color".into()));
        let lang = inherited_or("LANG", || Some("C.UTF-8".into()));
        let logname = inherited_or("LOGNAME", || user.clone());

        // Only when unseeded: a front end whose working directory is not the
        // process cwd states it first through `Shell::seed_cwd`.
        if self.context.cwd.0.is_none() {
            self.context.cwd.0 = crate::path::process_cwd();
        }

        let context = &mut self.context;
        let mut install = |k: &str, v: String| {
            context.set_env_var_or_keep(k, v);
        };
        // A host fact nothing binds stays unbound: seeding `HOME=.` once made
        // every `~` in the session mean "here".
        for (k, v) in [
            ("HOME", home),
            ("USER", user),
            ("PATH", path),
            ("SHELL", shell_path),
            ("TERM", term),
            ("LANG", lang),
            ("LOGNAME", logname),
        ] {
            if let Some(v) = v {
                install(k, v);
            }
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
        self.context.set_env_var("SHLVL", shlvl);

        // Compile-time facts, in `env` so rc can branch on the machine
        // without shelling out to `uname`.
        for (k, v) in [
            ("OS_NAME", crate::host::os_name()),
            ("OS_ARCH", crate::host::arch()),
            ("OS_FAMILY", crate::host::family()),
        ] {
            self.context.set_env_var(k, v);
        }
    }
}

/// The host's `key`, else `default()` where the host binds nothing. A value
/// that is not UTF-8 is `None`: no overlay entry, so children inherit its
/// bytes rather than a default in their place.
fn inherited_or(key: &str, default: impl FnOnce() -> Option<String>) -> Option<String> {
    match std::env::var(key) {
        Ok(v) => Some(v),
        Err(std::env::VarError::NotPresent) => default(),
        Err(std::env::VarError::NotUnicode(_)) => None,
    }
}
