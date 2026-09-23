//! What kind of process this one is, and what it owes the OS before it asks.
//!
//! `ral` re-execs itself to be several things that are not a shell: a wire
//! engine, a pipeline anchor, a bundled uutils tool, an OS-sandbox stage, a
//! test helper. Each is served here and exits, never reaching clap — which
//! would reject the argv that summoned it. Exarch opens the same way, through
//! `exarch::dispatch_pre_main`.

use crate::cli::Mode;
use ral_core::diagnostic;
use std::process::ExitCode;

/// What a `ral` process turned out to be.
pub(crate) enum Invocation {
    /// The shell itself, in the mode its arguments name.
    Shell(Mode),
    /// Not the shell: a re-exec child that has already done its work, or an
    /// invocation refused before it could become one. Exit with this code.
    Exit(ExitCode),
}

/// The shell inherits the caller's environment and must not run with
/// privileges the user did not request — and neither must any of the re-exec
/// children below, so this is asked first of all.
pub(crate) fn refuse_setuid() {
    #[cfg(unix)]
    if rustix::process::geteuid() != rustix::process::getuid() {
        eprintln!("ral: refusing to run setuid");
        #[allow(
            clippy::disallowed_methods,
            reason = "`main`'s first statement, asked before any re-exec child is served: nothing is booted yet, so nothing is held"
        )]
        std::process::exit(1);
    }
}

/// The process-wide dispositions every `ral` adopts, shell and re-exec child
/// alike.
pub(crate) fn adopt_process_dispositions() {
    #[cfg(windows)]
    ral_core::io::enable_virtual_terminal_processing();

    // Restore SIGPIPE to SIG_DFL once at startup so bundled uutils (this same
    // binary re-exec'd as `--ral-bundled-tool`) and the pipeline anchor see
    // the default disposition.
    #[cfg(unix)]
    ral_core::uutils::init_signal_dispositions();
}

/// Serve whichever re-exec child this process is, or read argv as a shell
/// invocation.
pub(crate) fn identify() -> Invocation {
    // The engine never returns: it takes the process over on fd 3.
    #[cfg(unix)]
    if std::env::args().any(|a| a == "--engine") {
        ral_core::engine::run_engine(&engine::INSTALLERS);
    }

    // Served off raw argv, which these read for themselves.
    if let Some(code) = ral_core::try_run_pipeline_anchor() {
        return Invocation::Exit(ExitCode::from(code));
    }
    if let Some(code) = ral_core::test_helper::try_run_test_helper() {
        return Invocation::Exit(ExitCode::from(code));
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let argv = match ral_core::sandbox::early_init(&argv) {
        Ok(stripped) => stripped,
        Err(e) => {
            diagnostic::cmd_error("ral", &e);
            return Invocation::Exit(ExitCode::from(1));
        }
    };

    // Served after the strip: a `--sandbox-projection` child enters the OS
    // sandbox first, then runs the target confined.
    if let Some(code) = ral_core::sandbox::serve_sandbox_exec(&argv) {
        return Invocation::Exit(ExitCode::from(code));
    }
    if let Some(code) = ral_core::try_run_bundled_tool(&argv) {
        return Invocation::Exit(ExitCode::from(code));
    }

    Invocation::Shell(Mode::from_argv(&argv))
}

/// What an engine of this binary boots into: `repl` for the interactive
/// session, `batch` for a script.
pub(crate) mod engine {
    use crate::boot_door;
    use ral_core::engine::{Booted, EngineInstaller};
    use ral_core::protocol::Attach;
    use ral_core::record;
    use ral_core::serial::datum::Datum as _;

    pub(crate) static INSTALLERS: [EngineInstaller; 2] = [
        EngineInstaller {
            tag: REPL,
            boot: boot_repl,
            narrow: no_seeded_children,
        },
        EngineInstaller {
            tag: BATCH,
            boot: boot_batch,
            narrow: no_seeded_children,
        },
    ];

    pub(crate) const REPL: &str = "repl";
    pub(crate) const BATCH: &str = "batch";

    /// The REPL surface: the batch surface plus the editor builtins and the
    /// plugin doors.
    fn repl_surface() -> ral_core::HostSurface {
        let mut surface = batch_surface();
        surface
            .statics
            .extend([crate::repl::ED_BUILTINS, crate::repl::PLUGIN_DOORS]);
        surface
    }

    /// The batch surface: core plus `watch` and `surface` (their docs say why
    /// they are host-installed) and the boot door. `--check` types against
    /// the same value, so the two agree by construction.
    pub(crate) fn batch_surface() -> ral_core::HostSurface {
        ral_core::HostSurface {
            statics: vec![
                ral_core::builtins::WATCH_BUILTIN,
                ral_core::builtins::SURFACE_BUILTIN,
                boot_door::DOOR,
            ],
            captured: Vec::new(),
        }
    }

    /// The `repl` installer's `Attach.config`.
    pub(crate) struct ReplConfig {
        pub(crate) login: bool,
    }

    record!(ReplConfig { login: "login" });

    /// Everything an interactive session is before rc.
    fn boot_repl(attach: &Attach) -> Result<Booted, String> {
        let config = ReplConfig::decode(&attach.config)?;
        let mut shell =
            ral_core::boot::boot_shell(attach.terminal, &crate::PRELUDE, &repl_surface());
        shell.set_exit_hints(crate::platform::load_exit_hints());
        shell.set_interactive(true);
        let terminal = shell.terminal().to_value();
        shell.set_var("TERMINAL".into(), terminal);
        if config.login {
            set_login_umask();
        }
        boot_door::register(&mut shell)?;
        Ok(Booted {
            shell,
            keep: Box::new(()),
        })
    }

    fn set_login_umask() {
        #[cfg(unix)]
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    }

    /// Everything a script's session is before its boot dispatch.
    fn boot_batch(attach: &Attach) -> Result<Booted, String> {
        let config = BatchConfig::decode(&attach.config)?;
        let mut shell =
            ral_core::boot::boot_shell(attach.terminal, &crate::PRELUDE, &batch_surface());
        shell.set_exit_hints(crate::platform::load_exit_hints());
        shell.set_args(config.args);
        boot_door::register(&mut shell)?;
        Ok(Booted {
            shell,
            keep: Box::new(()),
        })
    }

    /// The `batch` installer's `Attach.config`.
    pub(crate) struct BatchConfig {
        pub(crate) args: Vec<String>,
    }

    record!(BatchConfig { args: "args" });

    /// The REPL hatches nothing — only exarch spawns agents, and only exarch
    /// has a base-tag lexicon to resolve a grant against — so this engine's
    /// grant policy is a refusal. Saying so is the point of
    /// `EngineInstaller`'s field: an engine with no policy is
    /// unrepresentable, and a seeded child that reaches one is told why
    /// rather than silently admitted.
    fn no_seeded_children(
        _grant: &str,
        _cwd: &std::path::Path,
    ) -> Result<ral_core::types::Capabilities, String> {
        Err(
            "the ral shell's engine spawns no child engines, so it has no grant policy to hold \
             one to — was this meant to run under exarch?"
                .to_string(),
        )
    }
}
