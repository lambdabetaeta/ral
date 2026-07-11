//! Minimal stdin frontend for dumb terminals and `RAL_INTERACTIVE_MODE=minimal`.
//!
//! Bypasses rustyline entirely; reads from canonical stdin with no
//! raw-mode termios, no DECSET sequences, and no line editing — just
//! `read_line` with a `> ` continuation prompt.  Independent of the
//! plugin runtime; ghost text, highlights, and plugin keybindings are
//! unavailable here.

use ral_core::{Shell, diagnostic};
use std::io::{BufRead, Write};

use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, History, Read};

pub(in crate::repl) struct MinimalFrontend {
    history: History,
}

impl MinimalFrontend {
    pub(in crate::repl) fn new() -> Self {
        Self {
            history: History::load(),
        }
    }
}

impl Frontend for MinimalFrontend {
    fn read(
        &mut self,
        _shell: &mut Shell,
        prompt: &PromptText,
        _pending: Option<EditBuffer>,
        #[cfg(unix)] _jobs: &std::sync::Arc<std::sync::Mutex<crate::jobs::JobTable>>,
        #[cfg(feature = "structural")] _worksheet: &crate::repl::worksheet::Worksheet,
    ) -> Read {
        let stdin = std::io::stdin();
        let write_prompt = |s: &[u8]| {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(s);
            let _ = out.flush();
        };

        // Read first line.  A fresh prompt has no partial buffer to abandon,
        // so this read carries no abort-byte detection: a bare Ctrl-D on an
        // empty line arrives as `Ok(0)` (EOF), and a control byte at the start
        // of a piped line is fed through as ordinary input for the session to
        // reject — only the continuation read below abandons a partial buffer.
        write_prompt(prompt.styled().as_bytes());
        let mut line = String::new();
        let first_read = stdin.lock().read_line(&mut line);
        let input = match first_read {
            Ok(0) => return Read::Eof,
            Ok(_) => line.trim_end_matches(['\n', '\r']).to_string(),
            Err(e) => {
                diagnostic::cmd_error("ral", &e.to_string());
                return Read::Eof;
            }
        };

        // Continuation: while `parser::needs_continuation` (driven by
        // `join_continuation`) reports the buffer incomplete — an unclosed
        // lexeme or an Incompleteness class awaiting more input — prompt for
        // and fold in the next line.  An EOF / read error, or a leading
        // end-of-transmission (NUL) or Ctrl-C byte, abandons the partial
        // buffer; both abort bytes are read off the raw first byte.
        let input = super::join_continuation(input, || {
            write_prompt(b"> ");
            let mut cont = String::new();
            let cont_read = stdin.lock().read_line(&mut cont);
            let first_byte = cont.as_bytes().first().copied();
            match cont_read {
                Ok(0) | Err(_) => super::Continuation::Discard,
                Ok(_) if first_byte == Some(0x00) => super::Continuation::Discard,
                Ok(_) if first_byte == Some(0x03) => {
                    ral_core::process::clear();
                    super::Continuation::Discard
                }
                Ok(_) => super::Continuation::Line(cont.trim_end_matches(['\n', '\r']).to_string()),
            }
        });

        Read::Line(input)
    }

    fn add_history(&mut self, entry: &str) {
        self.history.add(entry);
    }

    fn save_history(&mut self) {
        self.history.save();
    }
}
