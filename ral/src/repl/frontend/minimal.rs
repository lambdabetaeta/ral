//! Minimal stdin frontend for dumb terminals and `RAL_INTERACTIVE_MODE=minimal`.
//!
//! Bypasses rustyline entirely; reads from canonical stdin with no
//! raw-mode termios, no DECSET sequences, and no line editing — just
//! `read_line` with a `> ` continuation prompt.  Independent of the
//! plugin runtime; ghost text, highlights, and plugin keybindings are
//! unavailable here.

use ral_core::{Shell, diagnostic};
use std::io::{BufRead, Write};

use super::super::config::dirs_history;
use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read};

pub(in crate::repl) struct MinimalFrontend {
    history: Vec<String>,
    /// Count of entries loaded from disk at construction; everything past
    /// it is this session's contribution, appended (not rewritten) on save
    /// so concurrent sessions do not clobber each other's history.
    persisted: usize,
    history_path: Option<String>,
}

impl MinimalFrontend {
    pub(in crate::repl) fn new() -> Self {
        let history_path = dirs_history();
        let history: Vec<String> = history_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default();
        let persisted = history.len();
        Self {
            history,
            persisted,
            history_path,
        }
    }
}

impl Frontend for MinimalFrontend {
    fn read(
        &mut self,
        _shell: &mut Shell,
        prompt: &PromptText,
        _pending: Option<EditBuffer>,
    ) -> Read {
        let stdin = std::io::stdin();
        let write_prompt = |s: &[u8]| {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(s);
            let _ = out.flush();
        };

        // Read first line.
        write_prompt(prompt.styled().as_bytes());
        let mut line = String::new();
        let input = match stdin.lock().read_line(&mut line) {
            Ok(0) => return Read::Eof,
            Ok(_) => line.trim_end_matches(['\n', '\r']).to_string(),
            Err(e) => {
                diagnostic::cmd_error("ral", &e.to_string());
                return Read::Eof;
            }
        };

        // Continuation: while the input ends with a continuation token
        // (|, ?, =, if, elsif, else, ,) prompt for and fold in the next
        // line.  An EOF / read error, an end-of-transmission `\0`, or a
        // Ctrl-C byte abandons the partial buffer.
        let input = super::join_continuation(input, || {
            write_prompt(b"> ");
            let mut cont = String::new();
            match stdin.lock().read_line(&mut cont) {
                Ok(0) | Err(_) => super::Continuation::Discard,
                Ok(_) if cont.trim().starts_with('\0') => super::Continuation::Discard,
                Ok(_) if cont.as_bytes().first().copied() == Some(0x03) => {
                    // Ctrl-C byte.
                    ral_core::process::clear();
                    super::Continuation::Discard
                }
                Ok(_) => super::Continuation::Line(cont.trim_end_matches(['\n', '\r']).to_string()),
            }
        });

        Read::Line(input)
    }

    fn add_history(&mut self, entry: &str) {
        if self.history.last().is_none_or(|s| s != entry) {
            self.history.push(entry.to_string());
        }
    }

    fn save_history(&mut self) {
        let Some(path) = &self.history_path else {
            return;
        };
        let fresh = &self.history[self.persisted..];
        if fresh.is_empty() {
            return;
        }
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            for entry in fresh {
                let _ = writeln!(file, "{entry}");
            }
        }
        self.persisted = self.history.len();
    }
}
