//! What kind of process this one is, and what it owes the OS before it asks.
//!
//! `ral` re-execs itself to be several things that are not a shell: a wire
//! engine, a pipeline anchor, a bundled uutils tool, an OS-sandbox stage, a
//! test helper. Core's `invocation::serve` serves each and exits, never reaching
//! clap — which would reject the argv that summoned it. Exarch opens through
//! the same dispatch.

use crate::cli::Mode;
#[cfg(unix)]
use crate::site::{Silent, site};
use ral_core::errln;
use std::ffi::OsString;
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
        errln!("ral: refusing to run setuid");
        site!(Silent::SetuidExit, std::process::exit(1));
    }
}

/// The process-wide dispositions every `ral` adopts: SIGPIPE stays ignored in
/// the shell, and a re-exec'd tool resets it for itself.
pub(crate) fn adopt_process_dispositions() {
    #[cfg(windows)]
    ral_core::io::terminal::enable_virtual_terminal_processing();
}

/// Serve whichever re-exec child this process is, or read argv as a shell
/// invocation.
pub(crate) fn identify() -> Invocation {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(code) = ral_core::invocation::serve(&ral_core::classify(&argv), &engine::INSTALLERS)
    {
        return Invocation::Exit(ExitCode::from(code));
    }
    match text_args(argv) {
        Ok(args) => Invocation::Shell(Mode::from_argv(&args)),
        Err(refusal) => {
            errln!("{refusal}");
            Invocation::Exit(ExitCode::from(2))
        }
    }
}

/// argv as text. ral's arguments become strings, so one that is not UTF-8 is
/// refused rather than mangled.
fn text_args(argv: Vec<OsString>) -> Result<Vec<String>, String> {
    argv.into_iter()
        .enumerate()
        .map(|(i, arg)| {
            arg.into_string().map_err(|arg| {
                // `{arg:?}`, not `.display()`: `Display` would paper over the
                // fault with the very U+FFFD substitution being refused.
                #[allow(
                    clippy::unnecessary_debug_formatting,
                    reason = "the escaped, lossless spelling is what names a non-UTF-8 argument"
                )]
                let quoted = format!("{arg:?}");
                format!("ral: argument {} is not UTF-8 text: {quoted}", i + 1)
            })
        })
        .collect()
}

/// What an engine of this binary boots into: `repl` for the interactive
/// session, `batch` for a script.
pub(crate) mod engine {
    use crate::boot_builtin;
    use ral_core::Runtime;
    use ral_core::data::datum::Datum as _;
    use ral_core::engine::EngineInstaller;
    use ral_core::protocol::{EngineSpec, Root};
    use ral_core::record;
    use ral_core::shell::boot::boot_runtime;

    pub(crate) static INSTALLERS: [EngineInstaller; 2] = [
        EngineInstaller {
            tag: REPL,
            runtime: repl_runtime,
            root: repl_root,
            narrow: no_seeded_children,
        },
        EngineInstaller {
            tag: BATCH,
            runtime: batch_runtime,
            root: batch_root,
            narrow: no_seeded_children,
        },
    ];

    pub(crate) const REPL: &str = "repl";
    pub(crate) const BATCH: &str = "batch";

    /// The REPL surface: the batch surface plus the editor builtins and the
    /// plugin load builtins.
    fn repl_surface() -> ral_core::HostSurface {
        let mut surface = batch_surface();
        surface.statics.extend([
            crate::repl::ED_BUILTINS,
            crate::repl::PLUGIN_LOAD_BUILTINS,
            crate::repl::external_editor::BUILTINS,
        ]);
        surface
    }

    /// The batch surface: core plus `watch` and `surface` (their docs say why
    /// they are host-installed) and the boot builtin. `--check` types against
    /// the same value, so the two agree by construction.
    pub(crate) fn batch_surface() -> ral_core::HostSurface {
        ral_core::HostSurface {
            statics: vec![
                ral_core::shell::builtins::WATCH_BUILTIN,
                ral_core::shell::builtins::SURFACE_BUILTIN,
                boot_builtin::BUILTINS,
            ],
            captured: Vec::new(),
        }
    }

    /// The REPL's process half.
    fn repl_runtime(spec: &EngineSpec) -> Result<Runtime, String> {
        boot_runtime(spec.terminal, &crate::PRELUDE, &repl_surface())
    }

    /// Everything an interactive session is before rc.
    fn repl_root(_: &Root, shell: &mut ral_core::Shell) -> Result<(), String> {
        shell.set_exit_hints(crate::platform::load_exit_hints());
        shell.set_interactive(true);
        let terminal = ral_core::Value::from(&shell.terminal());
        shell.set_var("TERMINAL".into(), terminal);
        crate::repl::external_editor::register(shell)?;
        boot_builtin::register(shell)
    }

    /// The batch session's process half.
    fn batch_runtime(spec: &EngineSpec) -> Result<Runtime, String> {
        boot_runtime(spec.terminal, &crate::PRELUDE, &batch_surface())
    }

    /// Everything a script's session is before its boot dispatch.
    fn batch_root(root: &Root, shell: &mut ral_core::Shell) -> Result<(), String> {
        let config = BatchConfig::decode(&root.config)?;
        shell.set_exit_hints(crate::platform::load_exit_hints());
        shell.set_args(config.args);
        boot_builtin::register(shell)
    }

    /// The `batch` installer's `Root.config`.
    pub(crate) struct BatchConfig {
        pub(crate) args: Vec<String>,
    }

    record!(BatchConfig { args: "args" });

    /// The REPL makes no child engines — only exarch spawns agents, and only
    /// exarch has a base-tag lexicon to resolve a grant against — so this
    /// engine's grant policy is a refusal. Saying so is the point of
    /// `EngineInstaller`'s field: an engine with no policy is
    /// unrepresentable, and a seeded child that reaches one is told why
    /// rather than silently admitted.
    fn no_seeded_children(
        _grant: &str,
        _ctx: &ral_core::authority::grant::freeze::FreezeCtx,
    ) -> Result<ral_core::authority::policy::GrantLayer, String> {
        Err(
            "the ral shell's engine spawns no child engines, so it has no grant policy to hold \
             one to: was this meant to run under exarch?"
                .to_string(),
        )
    }
}

/// Diagnose the host values ral cannot read as text.
pub(crate) fn warn_unreadable_env() {
    for (key, consequence) in [
        (
            "PATH",
            "ral cannot search it and a command must be named by its path",
        ),
        ("HOME", "`~` is an error"),
    ] {
        if ral_core::host::var_os(key).is_some_and(|v| v.into_string().is_err()) {
            errln!(
                "ral: warning: {key} is not valid UTF-8, so {consequence}; programs ral starts still receive it unchanged"
            );
        }
    }
}
