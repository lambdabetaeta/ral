use ral_core::{Shell, diagnostic};
use rustyline::config::{BellStyle, Builder, CompletionType, EditMode};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Editor, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use super::complete::RalHelper;
use super::config::dirs_history;
use super::keybinding::{KeybindingOutcome, dispatch_keybinding};
use super::plugin::{
    HookEnvGuard, PluginRuntime, flush_pending_messages, lock, prepare_hook_env, snapshot_history,
    sync_plugins,
};

// ── Result types ─────────────────────────────────────────────────────────

pub(super) enum FrontendEnd {
    Interrupted,
    Eof,
}

pub(super) enum KeybindingResolution {
    Proceed(String),
    Requeue(String, usize),
}

// ── Trait ─────────────────────────────────────────────────────────────────

pub(super) trait Frontend {
    /// Called once per loop iteration before `readline`.
    /// Handles partial-line marker, plugin sync, and history snapshot.
    fn before_readline(&mut self, shell: &Shell);

    /// Read one logical line (prompting for continuation after `|`, `?`, `=`, etc.).
    fn readline(
        &mut self,
        prompt: &str,
        initial: Option<(String, usize)>,
        shell: &Shell,
    ) -> Result<String, FrontendEnd>;

    /// Resolve any pending keybinding action for the line just read.
    /// For frontends with no keybindings, always returns `Proceed`.
    fn resolve_keybinding(&mut self, input: String, shell: &mut Shell) -> KeybindingResolution;

    /// Drain one entry from the buffer stack (pushed by `_editor 'push'`).
    fn drain_buffer_stack(&mut self) -> Option<(String, usize)>;

    /// Flush any plugin diagnostics buffered during readline/keybinding
    /// dispatch.  The REPL loop calls this at points where the terminal is
    /// in a stable state (after the line-erase escape, before the next
    /// prompt) so each message lands on a durable line.  No-op for
    /// frontends that have no plugin runtime.
    fn flush_plugin_messages(&self);

    fn add_history(&mut self, entry: &str);
    fn save_history(&mut self);
}

// ── MinimalFrontend ───────────────────────────────────────────────────────
//
// Bypasses rustyline entirely; reads from canonical stdin with no raw-mode
// termios, no DECSET sequences, and no line editing.

pub(super) struct MinimalFrontend {
    history: Vec<String>,
    history_path: Option<String>,
}

impl MinimalFrontend {
    pub(super) fn new() -> Self {
        let history_path = dirs_history();
        let history = history_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default();
        Self {
            history,
            history_path,
        }
    }
}

impl Frontend for MinimalFrontend {
    fn before_readline(&mut self, _env: &Shell) {}

    fn readline(
        &mut self,
        prompt: &str,
        _initial: Option<(String, usize)>,
        _env: &Shell,
    ) -> Result<String, FrontendEnd> {
        let stdin = std::io::stdin();
        let write_prompt = |s: &[u8]| {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(s);
            let _ = out.flush();
        };

        // Read first line.
        write_prompt(prompt.as_bytes());
        let mut line = String::new();
        let mut input = match stdin.lock().read_line(&mut line) {
            Ok(0) => return Err(FrontendEnd::Eof),
            Ok(_) => line.trim_end_matches(['\n', '\r']).to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                ral_core::signal::clear();
                println!();
                return Err(FrontendEnd::Interrupted);
            }
            Err(e) => {
                diagnostic::cmd_error("ral", &e.to_string());
                return Err(FrontendEnd::Eof);
            }
        };

        // Continuation: if the line ends with a continuation token (|, ?, =, if,
        // elsif, else, ,) prompt for the next line and join it.
        while ral_core::parser::needs_continuation(&input) {
            write_prompt(b"> ");
            let mut cont = String::new();
            match stdin.lock().read_line(&mut cont) {
                Ok(0) | Err(_) => { input.clear(); break; }
                Ok(_) if cont.trim().starts_with('\0') => { input.clear(); break; }
                Ok(_) => {
                    let cont = cont.trim_end_matches(['\n', '\r']).to_string();
                    if cont.as_bytes().first().copied() == Some(0x03) {
                        // Ctrl-C byte
                        ral_core::signal::clear();
                        input.clear();
                        break;
                    }
                    input.push('\n');
                    input.push_str(&cont);
                }
            }
        }

        Ok(input)
    }

    fn resolve_keybinding(&mut self, input: String, _env: &mut Shell) -> KeybindingResolution {
        KeybindingResolution::Proceed(input)
    }

    fn drain_buffer_stack(&mut self) -> Option<(String, usize)> {
        None
    }

    fn flush_plugin_messages(&self) {}

    fn add_history(&mut self, entry: &str) {
        if self.history.last().is_none_or(|s| s != entry) {
            self.history.push(entry.to_string());
        }
    }

    fn save_history(&mut self) {
        if let Some(path) = &self.history_path {
            let content = self.history.join("\n");
            let _ = std::fs::write(
                path,
                if content.is_empty() {
                    content
                } else {
                    format!("{content}\n")
                },
            );
        }
    }
}

// ── RustylineFrontend ─────────────────────────────────────────────────────

pub(super) struct RustylineFrontend {
    pub(super) rl: Editor<RalHelper, DefaultHistory>,
    pub(super) runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) edit_mode: EditMode,
    history_path: Option<String>,
}

impl RustylineFrontend {
    pub(super) fn new(
        shell: &Shell,
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

        // Route stdout through rustyline's ExternalPrinter so background output
        // (from `watch` blocks) appears above the active prompt.
        // We cannot easily wire this back to shell here without taking `&mut Shell`,
        // so the caller patches shell.io.stdout after construction.

        let mut rl: Editor<RalHelper, DefaultHistory> = Editor::with_config(config).unwrap();
        rl.bind_sequence(
            KeyEvent(KeyCode::Char('d'), Modifiers::CTRL),
            EventHandler::Conditional(Box::new(super::plugin::CtrlDHandler)),
        );
        rl.set_helper(Some(helper));

        let history_path = dirs_history();
        if let Some(ref path) = history_path {
            let _ = rl.load_history(path);
        }

        Self {
            rl,
            runtime,
            edit_mode,
            history_path,
        }
    }
}

impl Frontend for RustylineFrontend {
    fn before_readline(&mut self, shell: &Shell) {
        // Print partial-line marker if previous command left the cursor mid-line.
        if shell.io.terminal.ui_round_trips_ok() {
            #[cfg(unix)]
            super::cursor::partial_line_marker();
            #[cfg(not(unix))]
            {
                let _ = std::io::stdout().write_all(b"\r\x1b[K");
                let _ = std::io::stdout().flush();
            }
        }

        if let Some(h) = self.rl.helper_mut() {
            h.refresh_commands(shell);
        }
        sync_plugins(shell, &self.runtime, &mut self.rl);
        snapshot_history(&self.rl, &self.runtime);
    }

    fn readline(
        &mut self,
        prompt: &str,
        initial: Option<(String, usize)>,
        shell: &Shell,
    ) -> Result<String, FrontendEnd> {
        prepare_hook_env(shell, &self.runtime, self.edit_mode);
        let _guard = HookEnvGuard(&self.runtime);

        let first = if let Some((text, cursor)) = initial {
            let n = cursor.min(text.len());
            self.rl
                .readline_with_initial(prompt, (&text[..n], &text[n..]))
        } else {
            self.rl.readline(prompt)
        };

        let first = match first {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) => return Err(FrontendEnd::Interrupted),
            Err(ReadlineError::Eof) => return Err(FrontendEnd::Eof),
            Err(e) => {
                diagnostic::cmd_error("ral", &e.to_string());
                return Err(FrontendEnd::Eof);
            }
        };

        if !ral_core::parser::needs_continuation(&first) {
            return Ok(first);
        }

        // Continuation: if the line ends with a continuation token, prompt for
        // the next line and join it.
        let mut buf = first;
        loop {
            match self.rl.readline("> ") {
                Ok(cont) => {
                    buf.push('\n');
                    buf.push_str(&cont);
                    if !ral_core::parser::needs_continuation(&buf) {
                        return Ok(buf);
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    ral_core::signal::clear();
                    return Ok(String::new());
                }
                Err(e) => {
                    diagnostic::cmd_error("ral", &e.to_string());
                    return Err(FrontendEnd::Eof);
                }
            }
        }
    }

    fn resolve_keybinding(&mut self, input: String, shell: &mut Shell) -> KeybindingResolution {
        let pk = lock(&self.runtime).pending_keybinding.take();
        let Some(pk) = pk else {
            return KeybindingResolution::Proceed(input);
        };
        match dispatch_keybinding(pk, &input, shell, &self.runtime, self.edit_mode) {
            KeybindingOutcome::Accept(line) => KeybindingResolution::Proceed(line),
            KeybindingOutcome::Edit(text, cursor) => KeybindingResolution::Requeue(text, cursor),
        }
    }

    fn drain_buffer_stack(&mut self) -> Option<(String, usize)> {
        lock(&self.runtime).buffer_stack.pop()
    }

    fn flush_plugin_messages(&self) {
        flush_pending_messages(&self.runtime);
    }

    fn add_history(&mut self, entry: &str) {
        let _ = self.rl.add_history_entry(entry);
    }

    fn save_history(&mut self) {
        if let Some(ref path) = self.history_path {
            let _ = self.rl.save_history(path);
        }
    }
}
