//! Rustyline-backed frontend with completion, plugin keys, ghost text,
//! highlights, and history.  The real editor for interactive sessions
//! on TTYs that support raw mode and ANSI.

use ral_core::carrier::Transport;
use ral_core::terminal;
use ral_core::terminal::TerminalState;
use rustyline::config::{BellStyle, Builder, CompletionType};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Editor, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::sync::Arc;

use super::super::complete::RalHelper;
use super::super::cursor::{erase_drawn, partial_line_marker};
use super::super::host::{Printer, ReplHost};
use super::super::keybinding::{KeybindingOutcome, dispatch_keybinding};
use super::super::plugin::rustyline::{snapshot_history, sync_plugins};
use super::super::plugin::{Keymap, lock, pop_buffer_stack, reset_editor_hooks};
use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read};
use ral_core::text::char_to_byte;

pub(in crate::repl) struct RustylineFrontend {
    rl: Editor<RalHelper, DefaultHistory>,
    host: Arc<ReplHost>,
    keymap: Keymap,
    terminal: TerminalState,
    history_path: Option<String>,
    /// The key sequences bound with rustyline, so a sync can unbind them.
    bound: Vec<KeyEvent>,
}

impl RustylineFrontend {
    pub(in crate::repl) fn new(
        engine: Arc<dyn Transport>,
        host: Arc<ReplHost>,
        terminal: TerminalState,
        keymap: Keymap,
        bell: BellStyle,
        history_path: Option<String>,
    ) -> Result<Self, ReadlineError> {
        let config = Builder::new()
            .edit_mode(keymap.into())
            .bell_style(bell)
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(false)
            .completion_prompt_limit(30)
            .build();

        let mut rl: Editor<RalHelper, DefaultHistory> = Editor::with_config(config)?;
        rl.bind_sequence(
            KeyEvent(KeyCode::Char('d'), Modifiers::CTRL),
            EventHandler::Conditional(Box::new(super::super::plugin::rustyline::CtrlDHandler)),
        );
        rl.set_helper(Some(RalHelper::new(engine, host.clone(), terminal)));
        if let Some(path) = &history_path {
            let _ = rl.load_history(path);
        }

        Ok(Self {
            rl,
            host,
            keymap,
            terminal,
            history_path,
            bound: Vec::new(),
        })
    }
}

impl Frontend for RustylineFrontend {
    fn read(
        &mut self,
        engine: &dyn Transport,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
    ) -> Read {
        // Pre-readline housekeeping: partial-line marker, plugin sync,
        // helper refresh (cheap engine state only — the `PATH` enumeration
        // waits for the first Tab, so nothing here delays the prompt), history
        // snapshot, editor-hook reset.
        if self.terminal.ui_controls_ok() {
            partial_line_marker();
        }
        if let Some(h) = self.rl.helper_mut() {
            h.refresh();
        }
        let runtime = &self.host.runtime;
        sync_plugins(runtime, &mut self.rl, &mut self.bound);
        snapshot_history(&self.rl, runtime);
        reset_editor_hooks(runtime, self.keymap);

        // Caller-supplied pending wins; otherwise fall through to a buffer
        // pushed onto the stack by `_ed-push`.  The editor owns the whole
        // buffer, continuation lines included.
        let rl_prompt = (prompt.raw(), prompt.styled());
        let read = match pending.or_else(|| pop_buffer_stack(runtime)) {
            // `EditBuffer.cursor` counts characters; rustyline splits by byte.
            Some(buf) => {
                let n = char_to_byte(&buf.text, buf.cursor);
                self.rl
                    .readline_with_initial(&rl_prompt, (&buf.text[..n], &buf.text[n..]))
            }
            None => self.rl.readline(&rl_prompt),
        };
        let raw = match read {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) => return Read::Interrupt,
            Err(ReadlineError::Eof) => return Read::End(0),
            Err(e) => {
                terminal::cmd_error("ral", &e.to_string());
                return Read::End(1);
            }
        };

        // A plugin key submits via `AcceptLine`, so it sees the full buffer.
        // No pending key: the line is ready; Accept likewise; Edit re-queues
        // the buffer for the next read.
        let Some(pk) = lock(runtime).keybindings.pending.take() else {
            return Read::Line(raw);
        };
        match dispatch_keybinding(&pk, &raw, engine, &self.host, self.keymap) {
            KeybindingOutcome::Accept(line) => Read::Line(line),
            KeybindingOutcome::Edit(text, cursor) => {
                if self.terminal.ui_controls_ok() {
                    erase_drawn(prompt.raw(), &raw);
                }
                Read::Edit(EditBuffer { text, cursor })
            }
        }
    }

    fn add_history(&mut self, entry: &str) {
        let _ = self.rl.add_history_entry(entry);
    }

    /// Appends only this session's entries, so concurrent sessions do not
    /// clobber each other's history.
    fn save_history(&mut self) -> Result<(), String> {
        let Some(path) = &self.history_path else {
            return Ok(());
        };
        self.rl
            .append_history(path)
            .map_err(|e| format!("could not save history to {path}: {e}"))
    }

    fn printer(&mut self) -> Option<Printer> {
        self.rl
            .create_external_printer()
            .ok()
            .map(|p| Box::new(p) as Printer)
    }
}
