//! A table from (command basename, exit status) to a short explanation, read
//! by `Error::from_command_failure`.
//!
//! Finding the file is the host's job; parse the text here and install with
//! `Shell::set_exit_hints`.
//!
//! One entry per line, `<command> <status> <hint text>`; `*` matches any command,
//! and `#` lines and blanks are ignored.

use std::collections::HashMap;

/// Hints by command basename, then exit status.
#[derive(Default)]
pub struct ExitHints(HashMap<String, HashMap<i32, String>>);

impl ExitHints {
    /// Parse a hint table; malformed lines are skipped rather than reported.
    pub fn from_text(text: &str) -> Self {
        let mut table: HashMap<String, HashMap<i32, String>> = HashMap::new();
        for (cmd, status, hint) in text.lines().filter_map(entry) {
            // Keyed on the basename, as `lookup` is, so a full-path entry still matches.
            let by_status = table.entry(crate::path::basename(cmd).into()).or_default();
            by_status.insert(status, hint.into());
        }
        Self(table)
    }

    /// Hint for a command's exit status: the command's own entry, else the wildcard.
    ///
    /// Signals never reach here: the caller consults this only for
    /// `CommandFailure::ExitCode`, so no status is ever a 128+N encoding.
    pub(crate) fn lookup(&self, cmd: &str, status: i32) -> Option<String> {
        [crate::path::basename(cmd), "*"]
            .into_iter()
            .find_map(|name| self.0.get(name)?.get(&status))
            .cloned()
    }
}

fn entry(line: &str) -> Option<(&str, i32, &str)> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let ws = |c: char| c.is_ascii_whitespace();
    let (cmd, rest) = line.split_once(ws)?;
    let (status, hint) = rest.trim_start().split_once(ws)?;
    let hint = hint.trim_start();
    (!hint.is_empty()).then_some((cmd, status.parse().ok()?, hint))
}
