//! Rustyline-backed frontend with completion, plugin keys, ghost text,
//! highlights, and history.  The real editor for interactive sessions
//! on TTYs that support raw mode and ANSI.

use ral_core::carrier::Transport;
use ral_core::data::FOValue;
use ral_core::io::terminal;
use ral_core::io::terminal::TerminalState;
use ral_core::shell::runtime::HookName;
use ral_core::sync::LockExt as _;
use rustyline::config::{BellStyle, Builder, CompletionType};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Cmd, Editor, Event, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::super::complete::RalHelper;
use super::super::cursor::{erase_drawn, partial_line_marker};
use super::super::host::{Outcome, Printer, ReplHost};
use super::super::keybinding::{KeybindingOutcome, dispatch_keybinding};
use super::super::plugin::rustyline::{snapshot_history, sync_plugins};
use super::super::plugin::{Keymap, lock, pop_buffer_stack, reset_editor_hooks};
use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read, report_invalid_utf8};
use ral_core::text::char_to_byte;

pub(in crate::repl) struct RustylineFrontend {
    rl: Editor<RalHelper, DefaultHistory>,
    config: rustyline::Config,
    host: Arc<ReplHost>,
    keymap: Keymap,
    terminal: TerminalState,
    history_path: Option<PathBuf>,
    printer: Option<Printer>,
    /// The events bound with rustyline, so a sync can unbind them.
    bound: Vec<Event>,
}

impl RustylineFrontend {
    pub(in crate::repl) fn new(
        engine: Arc<dyn Transport>,
        host: Arc<ReplHost>,
        terminal: TerminalState,
        keymap: Keymap,
        bell: BellStyle,
        history_path: Option<PathBuf>,
    ) -> Result<Self, ReadlineError> {
        let config = Builder::new()
            .edit_mode(keymap.into())
            .bell_style(bell)
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(false)
            .completion_prompt_limit(30)
            .build();

        let mut rl: Editor<RalHelper, DefaultHistory> = Editor::with_config(config.clone())?;
        // Unix's printer requires the TTY path whose reader handles SIGINT.
        #[cfg(unix)]
        let printer = Some(Box::new(rl.create_external_printer()?) as Printer);
        #[cfg(windows)]
        let printer = rl
            .create_external_printer()
            .ok()
            .map(|p| Box::new(p) as Printer);
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
            config,
            host,
            keymap,
            terminal,
            history_path,
            printer,
            bound: Vec::new(),
        })
    }
}

fn bind_newlines(rl: &mut Editor<RalHelper, DefaultHistory>) {
    for key in [
        KeyEvent::ctrl('j'),
        KeyEvent(KeyCode::Enter, Modifiers::SHIFT),
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
    ] {
        rl.bind_sequence(key, Cmd::Newline);
    }
}

struct ExternalEditorKey(Arc<Mutex<Option<usize>>>);

impl rustyline::ConditionalEventHandler for ExternalEditorKey {
    fn handle(
        &self,
        _event: &Event,
        _repeat: rustyline::RepeatCount,
        _positive: bool,
        ctx: &rustyline::EventContext,
    ) -> Option<Cmd> {
        *self.0.lock_ignore_poison() = Some(ctx.pos());
        Some(Cmd::AcceptLine)
    }
}

fn edited_buffer(original: String, cursor_byte: usize, outcome: Outcome) -> EditBuffer {
    match outcome {
        Outcome::Settled(FOValue::String { value: text }) => EditBuffer {
            cursor: text.chars().count(),
            text,
        },
        outcome => {
            let reason = match outcome {
                Outcome::Faulted(reason) | Outcome::Walled(reason) | Outcome::Stopped(reason) => {
                    reason
                }
                Outcome::Exited => "the editor hook exited without returning a buffer".into(),
                Outcome::Settled(_) => "the editor hook did not return text".into(),
            };
            terminal::cmd_error("external editor", &reason);
            EditBuffer {
                cursor: original[..cursor_byte].chars().count(),
                text: original,
            }
        }
    }
}

impl Frontend for RustylineFrontend {
    fn read(
        &mut self,
        engine: &dyn Transport,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
        #[cfg(unix)] input: &mut super::Input,
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
        // Reserved editing gestures cannot become plugin submissions.
        bind_newlines(&mut self.rl);
        let external_editor = Arc::new(Mutex::new(None));
        for event in [
            Event::from(KeyEvent::ctrl('g')),
            Event::KeySeq(vec![KeyEvent::ctrl('x'), KeyEvent::ctrl('e')]),
        ] {
            self.rl.bind_sequence(
                event,
                EventHandler::Conditional(Box::new(ExternalEditorKey(external_editor.clone()))),
            );
        }
        snapshot_history(&self.rl, runtime);
        reset_editor_hooks(runtime, self.keymap);

        // Caller-supplied pending wins; otherwise fall through to a buffer
        // pushed onto the stack by `_ed-push`.  The editor owns the whole
        // buffer, continuation lines included.
        let rl_prompt = (prompt.raw(), prompt.styled());
        #[cfg(unix)]
        let reading = match input.readline() {
            Ok(Some(reading)) => reading,
            Ok(None) => return Read::End(0),
            Err(e) => {
                terminal::cmd_error(
                    "ral",
                    &format!("could not prepare interruptible input: {e}"),
                );
                return Read::End(1);
            }
        };
        let read = match pending.or_else(|| pop_buffer_stack(runtime)) {
            // `EditBuffer.cursor` counts characters; rustyline splits by byte.
            Some(buf) => {
                let n = char_to_byte(&buf.text, buf.cursor);
                self.rl
                    .readline_with_initial(&rl_prompt, (&buf.text[..n], &buf.text[n..]))
            }
            None => self.rl.readline(&rl_prompt),
        };
        #[cfg(unix)]
        {
            drop(reading);
            if input.ended() {
                return Read::End(0);
            }
        }
        let raw = match read {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) => return Read::Interrupt,
            Err(ReadlineError::Eof) => return Read::End(0),
            Err(ReadlineError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                report_invalid_utf8(&e);
                return Read::Interrupt;
            }
            Err(e) => {
                terminal::cmd_error("ral", &e.to_string());
                return Read::End(1);
            }
        };

        let editor_cursor = external_editor.lock_ignore_poison().take();
        if let Some(cursor) = editor_cursor {
            if self.terminal.ui_controls_ok() {
                erase_drawn(prompt.raw(), &raw, &self.config);
            }
            let result = self.host.run_hook(
                engine,
                HookName::session("external-editor"),
                vec![FOValue::String { value: raw.clone() }],
                None,
                None,
            );
            return Read::Edit(edited_buffer(raw, cursor, result.outcome));
        }

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
                    erase_drawn(prompt.raw(), &raw, &self.config);
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
        self.rl.append_history(path).map_err(|e| {
            format!(
                "could not save history to {}: {e}",
                ral_core::path::shown(path)
            )
        })
    }

    fn printer(&mut self) -> Option<Printer> {
        self.printer.take()
    }
}
