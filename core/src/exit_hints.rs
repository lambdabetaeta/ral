//! A table from (command basename, exit status) to a short explanation, read
//! by `Error::from_command_failure`.
//!
//! Finding the file is the host's job; parse the text here and install with
//! `Shell::set_exit_hints`.
//!
//! One entry per line, `<command> <status> <hint text>`; `*` matches any command,
//! and `#` lines and blanks are ignored.

use std::collections::HashMap;

/// Table of (command basename, exit status) → hint.
#[derive(Default)]
pub struct ExitHints {
    table: HashMap<(String, i32), String>,
}

impl ExitHints {
    /// Parse a hint table; malformed lines are skipped rather than reported.
    pub fn from_text(text: &str) -> Self {
        Self { table: parse(text) }
    }

    /// Hint for a command's exit status: the command's own entry, else the wildcard.
    ///
    /// Signals never reach here — the caller consults this only for
    /// `CommandFailure::ExitCode`, so no status is ever a 128+N encoding.
    pub fn lookup(&self, cmd: &str, status: i32) -> Option<String> {
        let name = crate::path::basename(cmd);

        if let Some(h) = self.table.get(&(name.to_string(), status)) {
            return Some(h.clone());
        }
        if let Some(h) = self.table.get(&("*".to_string(), status)) {
            return Some(h.clone());
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
            // Key on the basename, as `lookup` does, so an entry written as a
            // full path still matches.
            let name = crate::path::basename(cmd);
            map.insert((name.to_string(), status), hint.to_string());
        }
    }
    map
}
