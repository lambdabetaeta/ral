//! Exit-code hint lookup.
//!
//! `ExitHints` is a pure lookup table mapping (command, exit-status) pairs to
//! short human-readable explanations.  File loading is the caller's concern;
//! populate the table via [`ExitHints::from_text`] and install it into [`Shell`].
//!
//! # Table format
//!
//! One entry per line: `<command> <status> <hint text>`.
//! `<command>` is the bare program name or `*` for any command.
//! Lines starting with `#` and blank lines are ignored.

use std::collections::HashMap;

/// Signal names for Unix signals 1–31.
#[cfg(unix)]
const SIGNAL_NAMES: [&str; 32] = [
    "",          // 0 — unused
    "SIGHUP",    // 1
    "SIGINT",    // 2
    "SIGQUIT",   // 3
    "SIGILL",    // 4
    "SIGTRAP",   // 5
    "SIGABRT",   // 6
    "SIGBUS",    // 7
    "SIGFPE",    // 8
    "SIGKILL",   // 9
    "SIGUSR1",   // 10
    "SIGSEGV",   // 11
    "SIGUSR2",   // 12
    "SIGPIPE",   // 13
    "SIGALRM",   // 14
    "SIGTERM",   // 15
    "SIGSTKFLT", // 16
    "SIGCHLD",   // 17
    "SIGCONT",   // 18
    "SIGSTOP",   // 19
    "SIGTSTP",   // 20
    "SIGTTIN",   // 21
    "SIGTTOU",   // 22
    "SIGURG",    // 23
    "SIGXCPU",   // 24
    "SIGXFSZ",   // 25
    "SIGVTALRM", // 26
    "SIGPROF",   // 27
    "SIGWINCH",  // 28
    "SIGIO",     // 29
    "SIGPWR",    // 30
    "SIGSYS",    // 31
];

/// Table of (command, status) → hint.
///
/// Load with [`ExitHints::from_text`]; install into [`crate::types::Shell`].
#[derive(Default)]
pub struct ExitHints {
    /// Key: (command_basename, status).  `"*"` matches any command.
    table: HashMap<(String, i32), String>,
}

impl ExitHints {
    /// Build a hint table from text in the standard format.
    pub fn from_text(text: &str) -> Self {
        Self { table: parse(text) }
    }

    /// Return a hint for the given command basename and exit status, or `None`.
    ///
    /// Lookup order:
    /// 1. Command-specific entry.
    /// 2. Wildcard (`*`) entry.
    /// 3. Signal decode for status 129+ (128 + signal number, Unix only).
    pub fn lookup(&self, cmd: &str, status: i32) -> Option<String> {
        let name = std::path::Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(cmd);

        if let Some(h) = self.table.get(&(name.to_string(), status)) {
            return Some(h.clone());
        }
        if let Some(h) = self.table.get(&("*".to_string(), status)) {
            return Some(h.clone());
        }

        #[cfg(unix)]
        if status > 128 {
            let sig = status - 128;
            if sig > 0 && (sig as usize) < SIGNAL_NAMES.len() {
                return Some(format!(
                    "killed by signal {} ({})",
                    sig, SIGNAL_NAMES[sig as usize]
                ));
            } else if sig > 0 {
                return Some(format!("killed by signal {}", sig));
            }
        }

        None
    }
}

fn parse(text: &str) -> HashMap<(String, i32), String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((cmd, rest)) = line.split_once(|c: char| c.is_ascii_whitespace()) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((status_str, hint)) = rest.split_once(|c: char| c.is_ascii_whitespace()) else {
            continue;
        };
        let hint = hint.trim_start();
        let Ok(status) = status_str.parse::<i32>() else {
            continue;
        };
        if !hint.is_empty() {
            map.insert((cmd.to_string(), status), hint.to_string());
        }
    }
    map
}
