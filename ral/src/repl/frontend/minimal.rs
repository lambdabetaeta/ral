//! Minimal stdin frontend for dumb terminals and `RAL_INTERACTIVE_MODE=minimal`.
//!
//! Bypasses rustyline's editor entirely; reads from canonical stdin with no
//! raw-mode termios, no DECSET sequences, and no line editing — just
//! `read_line` with a `> ` continuation prompt.  Independent of the
//! plugin runtime; ghost text, highlights, and plugin keybindings are
//! unavailable here.

use ral_core::carrier::Transport;
use ral_core::io::terminal;
use ral_core::process::interrupt_epoch;
use ral_core::syntax::parser::needs_continuation;
use rustyline::history::{FileHistory, History};
use std::io::Write;
use std::path::PathBuf;

use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read};

pub(in crate::repl) struct MinimalFrontend {
    history: FileHistory,
    path: Option<PathBuf>,
}

impl MinimalFrontend {
    pub(in crate::repl) fn new(path: Option<PathBuf>) -> Self {
        let mut history = FileHistory::new();
        if let Some(p) = &path {
            let _ = history.load(p);
        }
        Self { history, path }
    }
}

/// One backend-supplied continuation read, classified for [`join_continuation`].
enum Continuation {
    /// Another line to fold into the buffer.
    Line(String),
    /// Ctrl-C abandons the entire logical input.
    Interrupt,
    /// EOF / read error: abandon the partial buffer, yielding an empty input
    /// for the session to skip.
    Discard,
}

/// Fold continuation lines into `first` until the buffer is a complete
/// input, joining with newlines.  The backend owns the actual read via `next`.
fn join_continuation(first: String, mut next: impl FnMut() -> Continuation) -> Option<String> {
    let mut buf = first;
    while needs_continuation(&buf) {
        match next() {
            Continuation::Line(line) => {
                buf.push('\n');
                buf.push_str(&line);
            }
            Continuation::Interrupt => return None,
            Continuation::Discard => return Some(String::new()),
        }
    }
    Some(buf)
}

impl Frontend for MinimalFrontend {
    fn read(
        &mut self,
        _engine: &dyn Transport,
        prompt: &PromptText,
        _pending: Option<EditBuffer>,
        #[cfg(unix)] input: &mut super::Input,
    ) -> Read {
        #[cfg(windows)]
        let stdin = std::io::stdin();
        let epoch = interrupt_epoch();
        #[cfg_attr(windows, allow(unused_mut))]
        let mut read_line = |line: &mut String| {
            #[cfg(unix)]
            {
                input.read_line(line, epoch)
            }
            #[cfg(windows)]
            {
                stdin.read_line(line)
            }
        };
        let write_prompt = |s: &str| {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(s.as_bytes());
            let _ = out.flush();
        };
        let trim = |s: &str| s[..s.len() - ral_core::io::terminator_len(s.as_bytes())].to_owned();

        write_prompt(prompt.styled());
        let mut line = String::new();
        match read_line(&mut line) {
            Ok(0) => return Read::End(0),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Read::Interrupt,
            Err(e) => {
                terminal::cmd_error("ral", &e.to_string());
                return Read::End(1);
            }
        }
        if interrupt_epoch() != epoch {
            return Read::Interrupt;
        }

        join_continuation(trim(&line), || {
            write_prompt("> ");
            let mut cont = String::new();
            match read_line(&mut cont) {
                Ok(0) => Continuation::Discard,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Continuation::Interrupt,
                Err(e) => {
                    terminal::cmd_error("ral", &e.to_string());
                    Continuation::Discard
                }
                Ok(_) if interrupt_epoch() != epoch => Continuation::Interrupt,
                Ok(_) => Continuation::Line(trim(&cont)),
            }
        })
        .map_or(Read::Interrupt, Read::Line)
    }

    fn add_history(&mut self, entry: &str) {
        let _ = self.history.add(entry);
    }

    fn save_history(&mut self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        self.history.append(path).map_err(|e| {
            format!(
                "could not save history to {}: {e}",
                ral_core::path::shown(path)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Continuation, join_continuation};

    /// A complete first line is returned without reading any continuation.
    #[test]
    fn complete_input_reads_no_continuation() {
        let mut calls = 0;
        let out = join_continuation("echo hi".into(), || {
            calls += 1;
            Continuation::Discard
        });
        assert_eq!(out.as_deref(), Some("echo hi"));
        assert_eq!(calls, 0, "a complete line must not prompt for more");
    }

    /// Continuation lines are folded in with newlines until the buffer
    /// parses as complete.
    #[test]
    fn folds_lines_until_complete() {
        let mut rest = vec!["world"].into_iter();
        // A trailing pipe is an incomplete input; the next line completes it.
        let out = join_continuation("echo hi |".into(), || {
            Continuation::Line(rest.next().unwrap().to_string())
        });
        assert_eq!(out.as_deref(), Some("echo hi |\nworld"));
    }

    /// A trailing `?` is a continuation exactly as a trailing `|` is, and
    /// the joined buffer must then parse.
    #[test]
    fn folds_a_line_after_a_trailing_chain_question() {
        let mut rest = vec!["echo b"].into_iter();
        let out = join_continuation("echo a ?".into(), || {
            Continuation::Line(rest.next().unwrap().to_string())
        });
        assert_eq!(out.as_deref(), Some("echo a ?\necho b"));
        assert!(ral_core::syntax::parser::parse(&out.unwrap()).is_ok());
    }

    /// Discard (EOF / read error) abandons the partial buffer.
    #[test]
    fn discard_yields_empty() {
        let out = join_continuation("echo hi |".into(), || Continuation::Discard);
        assert_eq!(out.as_deref(), Some(""));
    }

    #[test]
    fn interrupt_abandons_the_entire_buffer_without_another_read() {
        let mut calls = 0;
        let out = join_continuation("echo hi |".into(), || {
            calls += 1;
            Continuation::Interrupt
        });
        assert_eq!(out, None);
        assert_eq!(calls, 1);
    }
}
