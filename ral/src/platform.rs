//! Cross-platform helpers for user directories and environment seeding.
//!
//! Centralised here so `main.rs` and the `repl/` submodules can reach them
//! through `crate::platform::*` without chaining `super::super::`.  Both
//! Unix and Windows env-var fallbacks are encoded explicitly.

use ral_core::exit_hints::ExitHints;
use ral_core::io::{InteractiveMode, TerminalState};
use ral_core::{Shell, Value, diagnostic};

/// Probe the terminal under the active `RAL_INTERACTIVE_MODE`, plumb
/// it into the diagnostic subsystem, and return both halves.  When
/// `warn` is set, an unrecognised mode value emits a shell warning;
/// callers in non-interactive modes pass `false` to stay quiet.
pub(crate) fn probe_terminal(warn: bool) -> (InteractiveMode, TerminalState) {
    let raw = std::env::var("RAL_INTERACTIVE_MODE").ok();
    let (mode, mode_warn) = InteractiveMode::parse(raw.as_deref());
    if warn && let Some(msg) = mode_warn {
        diagnostic::shell_warning(&msg);
    }
    let terminal = TerminalState::probe_with_mode(mode);
    diagnostic::set_terminal(&terminal);
    (mode, terminal)
}

/// Home directory: `$HOME` on Unix, `%USERPROFILE%` on Windows.
pub(crate) fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}

/// Current user name: `$USER` on Unix, `%USERNAME%` on Windows.
pub(crate) fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "?".into())
}

/// Base config directory: `$XDG_CONFIG_HOME`, then `$HOME/.config`, then `%APPDATA%`.
pub(crate) fn config_base() -> Option<String> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .or_else(|| {
            let h = home_dir();
            if h == "." {
                None
            } else {
                Some(format!("{h}/.config"))
            }
        })
        .or_else(|| std::env::var("APPDATA").ok())
}

/// Base data directory: `$XDG_DATA_HOME`, then `$HOME/.local/share`.
pub(crate) fn data_base() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let h = home_dir();
            if h == "." {
                None
            } else {
                Some(std::path::PathBuf::from(format!("{h}/.local/share")))
            }
        })
}

static DEFAULT_EXIT_HINTS: &str = include_str!("../../data/exit-hints.txt");

/// Load exit-code hints: user override in data dir, else the embedded default.
pub(crate) fn load_exit_hints() -> ExitHints {
    let text = data_base()
        .map(|mut p| {
            p.push("ral/exit-hints.txt");
            p
        })
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let text = if text.is_empty() {
        DEFAULT_EXIT_HINTS
    } else {
        &text
    };
    ExitHints::from_text(text)
}

pub(crate) fn default_path() -> String {
    if cfg!(windows) {
        "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem".into()
    } else {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()
    }
}

/// Seed well-known environment variables into `shell` from the process
/// environment, filling in sensible defaults for anything unset.
pub(crate) fn seed_default_env(shell: &mut Shell) {
    let home = home_dir();
    let user = user_name();
    let path = std::env::var("PATH").unwrap_or_else(|_| default_path());
    let shell_path = std::env::var("SHELL").unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
            .unwrap_or_else(|| "ral".into())
    });
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let lang = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
    let pwd =
        std::env::current_dir().map_or_else(|_| "/".into(), |p| p.to_string_lossy().to_string());
    let oldpwd = std::env::var("OLDPWD").unwrap_or_else(|_| pwd.clone());
    let logname = std::env::var("LOGNAME").unwrap_or_else(|_| user.clone());

    let mut install = |k: &str, v: String| {
        shell.dynamic
            .env_vars
            .entry(k.into())
            .or_insert_with(|| v.clone());
        shell.set(k.into(), Value::String(v));
    };

    for (k, v) in [
        ("HOME", home),
        ("USER", user),
        ("PATH", path),
        ("SHELL", shell_path),
        ("TERM", term),
        ("LANG", lang),
        ("PWD", pwd),
        ("OLDPWD", oldpwd),
        ("LOGNAME", logname),
    ] {
        install(k, v);
    }

    // Pass through multiplexer and terminal-identity vars if the parent set them.
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

    // SHLVL: always increment the inherited value.
    let shlvl = std::env::var("SHLVL")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1)
        .to_string();
    shell.dynamic.env_vars.insert("SHLVL".into(), shlvl.clone());
    shell.set("SHLVL".into(), Value::String(shlvl));

    shell.exit_hints = load_exit_hints();
}
