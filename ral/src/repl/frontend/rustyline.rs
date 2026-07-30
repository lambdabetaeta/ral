//! Rustyline-backed frontend with completion, plugin keys, ghost text,
//! highlights, and history.  The real editor for interactive sessions
//! on TTYs that support raw mode and ANSI.

use ral_core::{Shell, diagnostic};
use rustyline::config::{BellStyle, Builder, CompletionType, EditMode};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Editor, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::complete::RalHelper;
use super::super::config::dirs_history;
use super::super::keybinding::{KeybindingOutcome, dispatch_keybinding};
use super::super::plugin::{
    HookEnvGuard, PluginRuntime, flush_pending_messages, lock, pop_buffer_stack, prepare_hook_env,
    snapshot_history, sync_plugins,
};
use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read};
use ral_core::text::char_to_byte;

pub(in crate::repl) struct RustylineFrontend {
    rl: Editor<RalHelper, DefaultHistory>,
    pub(in crate::repl) runtime: Arc<Mutex<PluginRuntime>>,
    pub(in crate::repl) edit_mode: EditMode,
    history_path: Option<String>,
}

impl RustylineFrontend {
    pub(in crate::repl) fn new(
        shell: &mut Shell,
        edit_mode: EditMode,
        bell: BellStyle,
        runtime: Arc<Mutex<PluginRuntime>>,
    ) -> Self {
        let helper = RalHelper::new(shell, runtime.clone());
        let config = Builder::new()
            .edit_mode(edit_mode)
            .bell_style(bell)
            .completion_type(CompletionType::List)
            .completion_show_all_if_ambiguous(false)
            .completion_prompt_limit(30)
            .build();

        let mut rl: Editor<RalHelper, DefaultHistory> = Editor::with_config(config).unwrap();
        rl.bind_sequence(
            KeyEvent(KeyCode::Char('d'), Modifiers::CTRL),
            EventHandler::Conditional(Box::new(super::super::plugin::CtrlDHandler)),
        );
        rl.set_helper(Some(helper));

        let history_path = dirs_history();
        if let Some(ref path) = history_path {
            let _ = rl.load_history(path);
        }

        let mut frontend = Self {
            rl,
            runtime,
            edit_mode,
            history_path,
        };
        frontend.wire_external_printer(shell);
        frontend
    }

    /// Route the shell's stdout through rustyline's `ExternalPrinter` so
    /// background output (from `watch` blocks) prints above the active prompt
    /// instead of colliding with the line being edited.  A terminal that
    /// cannot supply an external printer leaves stdout as it was.
    fn wire_external_printer(&mut self, shell: &mut Shell) {
        let Ok(printer) = self.rl.create_external_printer() else {
            return;
        };
        use std::sync::Mutex as StdMutex;
        struct RustylineSink<P: rustyline::ExternalPrinter + Send>(StdMutex<P>);
        impl<P: rustyline::ExternalPrinter + Send + 'static> ral_core::io::ExternalWrite
            for RustylineSink<P>
        {
            fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
                let s = String::from_utf8_lossy(bytes).into_owned();
                if let Ok(mut p) = self.0.lock() {
                    p.print(s)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(())
            }
        }
        shell.set_stdout(ral_core::io::Sink::External(Arc::new(RustylineSink(
            StdMutex::new(printer),
        ))));
    }

    /// Run rustyline's `readline` (with or without an initial buffer) and
    /// continue reading until the resulting input no longer requires
    /// continuation.  Returns the final logical input, or [`Read::Interrupt`]
    /// / [`Read::Eof`] on terminal events from rustyline.
    fn readline_with_continuation(
        &mut self,
        prompt: &PromptText,
        initial: Option<EditBuffer>,
    ) -> Result<String, Read> {
        let rl_prompt = (prompt.raw(), prompt.styled());
        let first = if let Some(buf) = initial {
            // EditBuffer.cursor is a character offset; rustyline slices the
            // initial text by byte.  Convert before splitting so a cursor
            // inside multi-byte characters does not panic.
            let n = char_to_byte(&buf.text, buf.cursor);
            self.rl
                .readline_with_initial(&rl_prompt, (&buf.text[..n], &buf.text[n..]))
        } else {
            self.rl.readline(&rl_prompt)
        };

        let first = match first {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) => return Err(Read::Interrupt),
            Err(ReadlineError::Eof) => return Err(Read::Eof),
            Err(e) => {
                diagnostic::cmd_error("ral", &e.to_string());
                return Err(Read::Eof);
            }
        };

        // Continuation: while the buffer needs more input, prompt for and
        // fold in the next line.  Ctrl-C, Ctrl-D, and a read error all
        // abandon the partial buffer (yielding an empty line for the
        // session to skip) rather than ending the shell — an unterminated
        // input is the user's mistake to back out of, not grounds to log
        // them out.  This shares the `needs_continuation` loop with the
        // minimal frontend, where the prior divergence let Ctrl-D kill the
        // shell here but not there.
        Ok(super::join_continuation(first, || {
            match self.rl.readline("> ") {
                Ok(cont) => super::Continuation::Line(cont),
                Err(ReadlineError::Interrupted) => {
                    ral_core::process::clear();
                    super::Continuation::Discard
                }
                Err(ReadlineError::Eof) => super::Continuation::Discard,
                Err(e) => {
                    diagnostic::cmd_error("ral", &e.to_string());
                    super::Continuation::Discard
                }
            }
        }))
    }
}

impl Frontend for RustylineFrontend {
    fn read(
        &mut self,
        shell: &mut Shell,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
        #[cfg(unix)] _jobs: &Arc<Mutex<crate::jobs::JobTable>>,
        #[cfg(feature = "structural")] _worksheet: &crate::repl::worksheet::Worksheet,
    ) -> Read {
        // Pre-readline housekeeping: partial-line marker, plugin sync,
        // helper refresh (cheap shell state only — the `PATH` enumeration waits
        // for the first Tab, so nothing here delays the prompt), history
        // snapshot, hook env prep.
        if shell.terminal().ui_round_trips_ok() {
            super::super::cursor::partial_line_marker();
        }
        if let Some(h) = self.rl.helper_mut() {
            h.refresh(shell);
        }
        sync_plugins(&self.runtime, &mut self.rl);
        snapshot_history(&self.rl, &self.runtime);

        prepare_hook_env(shell, &self.runtime, self.edit_mode.into());
        let _guard = HookEnvGuard(self.runtime.clone());

        // Caller-supplied pending wins; otherwise fall through to a buffer
        // pushed onto the stack by `_ed-push`.
        let initial = pending.or_else(|| pop_buffer_stack(&self.runtime));

        let raw = match self.readline_with_continuation(prompt, initial) {
            Ok(s) => s,
            Err(end) => return end,
        };

        // Resolve any plugin keybinding that fired during readline.  No
        // pending key means the line is ready to evaluate; an Accept also
        // produces a ready line; an Edit re-queues the buffer for the next
        // read.  Plugin diagnostics buffered during readline or dispatch
        // are flushed on every return path so they land on a durable line
        // above the next prompt — after any line-erase escape we emit.
        let pk = lock(&self.runtime).keybindings.pending.take();
        let Some(pk) = pk else {
            flush_pending_messages(&self.runtime);
            return Read::Line(raw);
        };
        match dispatch_keybinding(&pk, &raw, shell, &self.runtime, self.edit_mode.into()) {
            KeybindingOutcome::Accept(line) => {
                flush_pending_messages(&self.runtime);
                Read::Line(line)
            }
            KeybindingOutcome::Edit(text, cursor) => {
                // Erase the stray newline rustyline emits on AcceptLine,
                // *then* flush plugin diagnostics so they land on a durable
                // line above the next prompt.  Order matters: printing
                // before the escape would have its line clobbered.
                if shell.terminal().ui_round_trips_ok() {
                    let _ = std::io::stdout().write_all(b"\x1b[A\r\x1b[K");
                    let _ = std::io::stdout().flush();
                }
                flush_pending_messages(&self.runtime);
                Read::Edit(EditBuffer { text, cursor })
            }
        }
    }

    fn add_history(&mut self, entry: &str) {
        let _ = self.rl.add_history_entry(entry);
    }

    fn save_history(&mut self) {
        if let Some(ref path) = self.history_path {
            // Append only the entries added this session rather than
            // rewriting the whole file, so two concurrent sessions do not
            // clobber each other's history.  Falls back to nothing useful
            // when the file does not yet exist; the first session's
            // `add_history_entry` then seeds it on the next append.
            let _ = self.rl.append_history(path);
        }
    }
}
