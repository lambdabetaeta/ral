//! Exit-code hint lookup.
//!
//! `ExitHints` is a pure lookup table mapping (command, exit-status) pairs to
//! short human-readable explanations.  File loading is the caller's concern;
//! populate the table via [`ExitHints::from_text`] and install it into [`crate::Shell`].
//!
//! # Table format
//!
//! One entry per line: `<command> <status> <hint text>`.
//! `<command>` is the bare program name or `*` for any command.
//! Lines starting with `#` and blank lines are ignored.

use std::collections::HashMap;

/// Table of (command, status) → hint.
///
/// Load with [`ExitHints::from_text`]; install into [`crate::types::Shell`].
#[derive(Default)]
pub struct ExitHints {
    /// Key: (`command_basename`, status).  `"*"` matches any command.
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
    ///
    /// Signal-terminated statuses are not decoded into synthetic hints.
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
            // Key on the basename, exactly as `lookup` does — a table entry
            // written as a full path (`/usr/bin/foo`) must still match a
            // lookup for that command, which is always basename-keyed.
            let name = crate::path::basename(cmd);
            map.insert((name.to_string(), status), hint.to_string());
        }
    }
    map
}
