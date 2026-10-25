//! Shared session bring-up helpers: terminal probing, the in-process attach,
//! and exit-code hints.
//!
//! Centralised here so the batch runner and the `repl/` submodules can reach
//! them through `crate::platform::*` without one path reaching into the
//! other's module.  Both Unix and Windows env-var fallbacks are encoded
//! explicitly.

use ral_core::data::FOValue;
use ral_core::io::terminal;
use ral_core::io::terminal::{InteractiveMode, TerminalState};
use ral_core::own::UserFile;
use ral_core::path::{EnginePath, XdgKind};
use ral_core::protocol::{Attach, EngineSpec, Root};
use ral_core::shell::runtime::ExitHints;

use crate::site::{Silent, site};

/// Probe the terminal under the active `RAL_INTERACTIVE_MODE`, plumb
/// it into the diagnostic subsystem, and return both halves.  When
/// `warn` is set, an unrecognised mode value emits a shell warning;
/// callers in non-interactive modes pass `false` to stay quiet.
pub(crate) fn probe_terminal(warn: bool) -> (InteractiveMode, TerminalState) {
    let (mode, terminal, mode_warn) = TerminalState::probe_from_env();
    if warn && let Some(msg) = mode_warn {
        terminal::shell_warning(&msg);
    }
    terminal.seat();
    (mode, terminal)
}

/// An in-process engine's attach: its cwd and home are this process's own.
///
/// # Errors
/// When the process has no working directory, which a session cannot start
/// without: every relative path it reads is read against one.
pub(crate) fn local_attach(
    installer: &str,
    terminal: TerminalState,
    config: FOValue,
) -> Result<Attach, String> {
    let home = site!(Silent::EngineHome, ral_core::host::home()).unwrap_or_default();
    let cwd = ral_core::host::cwd()
        .ok_or("the working directory is unavailable, so no relative path could be read")?;
    Ok(Attach::root(
        EngineSpec::new(installer, EnginePath::Host(home.into())),
        Root::new(EnginePath::Host(cwd.into_inner())).with_config(config),
    )
    .with_terminal(terminal))
}

static DEFAULT_EXIT_HINTS: &str = include_str!("../../data/exit-hints.txt");

/// Load exit-code hints: user override in data dir, else the embedded default.
/// No data base, or an override that cannot be read, is no override.
pub(crate) fn load_exit_hints() -> ExitHints {
    let text = UserFile::xdg("ral", XdgKind::Data, "exit-hints.txt")
        .ok()
        .and_then(|file| file.read().ok().flatten())
        .unwrap_or_default();
    let text = if text.is_empty() {
        DEFAULT_EXIT_HINTS
    } else {
        &text
    };
    ExitHints::from_text(text)
}

/// Clamp a shell exit code to the `0..=255` process byte-status range.
///
/// Ral carries exit codes as `i32` (an `exit` argument, a signal-derived
/// status, a cancel cause); the OS process status is a single byte. Every
/// mode — batch, `-c`, the REPL run loop — funnels its final code through
/// here so the clamp-and-narrow lives in one place.
pub(crate) fn exit_byte(code: i32) -> u8 {
    #[allow(
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255, a byte exit status"
    )]
    let byte = code.clamp(0, 255) as u8;
    byte
}
