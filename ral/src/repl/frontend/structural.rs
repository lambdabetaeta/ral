//! The structural frontend (`--surface structural`).
//!
//! A third [`Frontend`] alongside [`super::MinimalFrontend`] and
//! [`super::RustylineFrontend`].  Where those render a single editable row,
//! this renders a *projection of live program state* around the prompt: the
//! per-stage types of the pipeline being composed (the **typed spine**), the
//! session's `let` bindings with their types and value previews (the
//! **worksheet**), and every live spawn handle (the **handles matrix**).
//! The runtime already computes all three — the checker infers per-stage
//! types, the environment holds typed bindings, the concurrency runtime
//! holds handles — and the ordinary REPL throws them away unless they
//! constitute an error.  This frontend stops throwing them away.
//!
//! It draws into a ratatui *inline viewport* at the bottom of the normal
//! screen, so scrollback above is preserved and the prompt stays where
//! shells have always put it.  Raw mode and the viewport are entered per
//! `read` and left before the line is returned, so the session evaluates and
//! prints command output to the ordinary screen between reads.
//!
//! This is the keystone cut: the typed spine (live per-stage inference), a
//! read-only worksheet, and a handles matrix over both env-held spawn handles
//! and pgid jobs (Ctrl-Z, Unix only).  The reactive re-flow, fork,
//! detached-worker enumeration, and point-at-value matrix actions the design
//! proposal also describes are later parcels; the projections here read
//! runtime state and never mutate it.

use ral_core::Shell;
use ral_core::ir::{Comp, CompKind};
use ral_core::typecheck::{Scheme, fmt_scheme, fmt_ty};
use ral_core::types::HandleState;
use ral_core::{CompileOutcome, Value};

use ansi_to_tui::IntoText;
use prompt_editor::{EditMode, PromptEditor};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, History, Read};
#[cfg(unix)]
use crate::jobs::JobTable;
use crate::repl::complete::style_ratatui;
use crate::repl::completion::{self, Candidate, Sources};
use crate::repl::keybinding::{KeybindingOutcome, dispatch_keybinding};
use crate::repl::plugin::{
    HookEnvGuard, KeyChord, KeyName, Keymap, PendingKeybinding, PluginRuntime,
    flush_pending_messages, lock, pop_buffer_stack, prepare_hook_env, run_buffer_change_hooks,
};
use crate::repl::plugin_editor::{HighlightSpan, char_to_byte};
use crate::repl::worksheet::Worksheet;

// ── Palette ───────────────────────────────────────────────────────────────

const TYPE_HUE: Color = Color::Rgb(135, 200, 215); // cyan — types
const NAME_HUE: Color = Color::Rgb(165, 210, 155); // lime — binding names
const FLARE_HUE: Color = Color::Rgb(215, 110, 125); // red — type-error flare
const SLATE: Color = Color::Rgb(140, 150, 170); // dim chrome
const HANDLE_RUN: Color = Color::Rgb(220, 140, 175); // pink — running handle
const EFFECT_HUE: Color = Color::Rgb(225, 170, 110); // amber — effectful binding

/// Idle redraw cadence: the structural surface polls keys and redraws the
/// (constant, between-keystroke) projections at this interval.
const TICK: Duration = Duration::from_millis(120);

/// Upper bound on the inline viewport's height, so a session with many
/// bindings never swallows the whole screen — scrollback must stay visible.
const MAX_VIEWPORT: u16 = 18;

/// The completion menu shows at most this many candidate rows at once,
/// scrolling within them.  The lower band reserves room for this many rows so
/// a menu opened on a fresh session (empty worksheet) still has space to drop
/// down rather than being clipped to a two-row projection band.
const MENU_MAX_ROWS: u16 = 6;

/// An open completion menu: the ranked candidates from [`completion::complete`]
/// and the buffer span they replace.  Tab opens it (when more than one
/// candidate matches), Tab/↓ and ⇧Tab/↑ cycle the selection, Enter accepts the
/// selected candidate, and Esc — or any editing key — dismisses it.
struct Menu {
    candidates: Vec<Candidate>,
    selected: usize,
    /// Byte offset into the trigger row where the chosen replacement starts.
    replace_from: usize,
    /// The editor row the menu was opened on; accept aborts if the cursor has
    /// since left it.
    row: usize,
    /// Screen column the popup drops down under: the prompt prefix width plus
    /// the token's start column, so the list aligns under what is being typed.
    anchor_col: u16,
}

/// What [`StructuralFrontend::compose`]'s edit loop breaks with.  `Done` is a
/// finished read; `Keybinding` is a plugin key that fired and must be
/// dispatched once the viewport is gone (its handler may take the terminal via
/// `_ed-tui`, exactly as rustyline dispatches only after `readline` returns).
/// Kept internal to this frontend so the shared [`Read`] enum gains no
/// structural-only variant.
enum Composed {
    Done(Read),
    Keybinding(PendingKeybinding),
}

pub(in crate::repl) struct StructuralFrontend {
    history: History,
    /// The set of binding names present at the first `read` — the prelude
    /// and prompt bindings — so the worksheet shows only what the user has
    /// since defined.  Captured lazily because `new` has no shell.
    baseline: Option<HashSet<String>>,
    /// Whether the user asked for vi keys (`edit_mode: vi` in their ralrc).
    /// Reduced from `rustyline::config::EditMode` at construction so the rest
    /// of this frontend never sees rustyline's type.
    vi: bool,
    /// The plugin runtime, shared with the session and the other frontends.
    /// The compose loop drives buffer-change hooks and dispatches plugin
    /// keybindings through it, reusing the same neutral primitives the
    /// rustyline frontend does — so the in-editor plugin surface (ghost text,
    /// highlights, fzf/zoxide keys) works under `structural` too.
    runtime: Arc<Mutex<PluginRuntime>>,
}

impl StructuralFrontend {
    /// Construct the frontend, verifying the terminal supports raw mode (so
    /// the boot selector can fall back when it does not).  Loads persisted
    /// history like the other frontends.  `edit_mode` selects emacs vs. vi
    /// keybindings, reduced here to a plain flag.  `runtime` is the shared
    /// plugin runtime the in-editor surface drives.
    pub(in crate::repl) fn new(
        edit_mode: rustyline::config::EditMode,
        runtime: Arc<Mutex<PluginRuntime>>,
    ) -> io::Result<Self> {
        // Probe raw mode once: if the terminal cannot do it, the structural
        // surface cannot run and the caller degrades to a line editor.
        enable_raw_mode()?;
        disable_raw_mode()?;
        Ok(Self {
            history: History::load(),
            baseline: None,
            vi: matches!(edit_mode, rustyline::config::EditMode::Vi),
            runtime,
        })
    }

    /// The composition loop: enter raw mode + an inline viewport, edit the
    /// buffer while projecting the live shell, and leave the viewport before
    /// returning so the session's output prints to the ordinary screen.
    fn compose(
        &mut self,
        shell: &mut Shell,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
        #[cfg(unix)] jobs: &Arc<Mutex<JobTable>>,
        worksheet: &Worksheet,
    ) -> io::Result<Read> {
        // The worksheet and matrix read the env, which does not change while
        // the user composes (no evaluation happens here), so build them once.
        // Both project the same user bindings, so fold the scope once and
        // derive both from that single snapshot.  The matrix also takes a
        // snapshot of the pgid jobs (the session reaps the table each turn
        // before `read`), copied out under a brief lock that is dropped before
        // rendering.
        let baseline = self.baseline.get_or_insert_with(|| binding_names(shell));
        let user = user_bindings(shell, baseline);
        let ws_rows = worksheet_rows(&user, shell, worksheet);
        #[cfg(unix)]
        let jobs_snapshot = job_rows(jobs);
        #[cfg(not(unix))]
        let jobs_snapshot = Vec::new();
        let matrix = matrix_rows(&user, jobs_snapshot);

        // The styled prompt, parsed into spans once and split on its newlines:
        // ansi-to-tui turns the SGR escapes into ratatui styling.  A parse
        // failure degrades to the ANSI-stripped raw text — never to a blank
        // prompt.  A multi-line prompt draws its lead rows above the editor.
        let prompt_lines = split_prompt(prompt);

        // The in-editor plugin surface, set up exactly as the rustyline
        // frontend sets it up before `readline` — reusing the same neutral
        // primitives, not a parallel implementation.  The session-supplied
        // `pending` wins; otherwise a buffer pushed by `_ed-push` (fzf-cd /
        // zoxide save the current line, run, then accept a `cd`, and the
        // saved line is restored here on the next read) is popped.  The hook
        // shell + its guard, and the newest-first history snapshot that
        // autosuggestion's `_ed-history` reads, are prepared up front.
        let keymap = if self.vi { Keymap::Vi } else { Keymap::Emacs };
        let initial = pending.or_else(|| pop_buffer_stack(&self.runtime));
        prepare_hook_env(shell, &self.runtime, keymap);
        let _guard = HookEnvGuard(self.runtime.clone());
        lock(&self.runtime).hooks.history = self.history.entries().iter().rev().cloned().collect();

        let mut prompt = PromptEditor::new(if self.vi {
            EditMode::Vi
        } else {
            EditMode::Emacs
        });
        if let Some(p) = &initial {
            prompt.set_text(&p.text);
            prompt.place_char_offset(p.cursor);
        }
        let mut hist_pos: Option<usize> = None;
        let mut draft = String::new();
        // Lazily-built completion candidate snapshot, and the open menu (if
        // any).  The snapshot is built on the first Tab and reused for the rest
        // of this compose — no evaluation happens while composing, so the
        // commands/variables/cwd it captures cannot change underfoot.
        let mut sources: Option<Sources> = None;
        let mut menu: Option<Menu> = None;
        // The plugin keybinding chords, built once on the first keypress (like
        // `sources`): the runtime does not change while composing.
        let mut chords: Option<Vec<(String, usize, KeyChord)>> = None;

        enable_raw_mode()?;
        let (_cols, rows) = size().unwrap_or((80, 24));
        // Size the viewport to its content at entry: the spine, the prompt's
        // own (possibly multi-line) rows, and the projections.  Clamping to
        // `rows - 1` hugs the bottom — the prompt sits where a shell prompt
        // always sits — and `MAX_VIEWPORT` keeps scrollback in view.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
        )]
        let lead_rows = prompt_lines.lead.len() as u16;
        let height = viewport_height(
            lead_rows,
            &prompt,
            &ws_rows,
            &matrix,
            rows,
        );
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        // The spine is recomputed only when the buffer changes; an idle
        // redraw reuses the cached one.
        let mut spine = Spine::Empty;
        let mut last_buf: Option<String> = None;

        let result = loop {
            let s = prompt.text();
            if last_buf.as_deref() != Some(s.as_str()) {
                spine = build_spine(&s, shell);
                last_buf = Some(s.clone());
            }
            // Drive plugin buffer-change hooks (fish-style ghost text,
            // highlight spans), then read back what they produced.  The driver
            // dedups on (text, cursor) internally, so an idle redraw between
            // keystrokes re-runs no plugin code.  The ghost is hidden while the
            // completion menu owns the lower band.
            run_buffer_change_hooks(&self.runtime, &s, prompt.cursor_byte_offset());
            let (ghost, highlights) = {
                let rt = lock(&self.runtime);
                (rt.hooks.ghost.clone(), rt.hooks.highlights.clone())
            };
            terminal.draw(|frame| {
                render(
                    frame,
                    &prompt_lines,
                    &mut prompt,
                    &spine,
                    &ws_rows,
                    &matrix,
                    menu.as_ref(),
                    ghost.as_deref().filter(|_| menu.is_none()),
                    &highlights,
                );
            })?;

            if !event::poll(TICK)? {
                continue;
            }
            let Event::Key(k) = event::read()? else {
                continue;
            };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

            // While the completion menu is open it owns the keys: ↓/Tab and
            // ↑/⇧Tab cycle the selection, Enter accepts, Esc or Ctrl-C dismiss.
            // Any other key dismisses the menu and falls through to ordinary
            // editing, so typing continues seamlessly.  Handled before the
            // normal dispatch so Tab/Enter/Esc mean menu actions here.
            if menu.is_some() {
                let n = menu.as_ref().map_or(0, |m| m.candidates.len());
                match k.code {
                    KeyCode::Tab | KeyCode::Down => {
                        let m = menu.as_mut().unwrap();
                        m.selected = (m.selected + 1) % n;
                        continue;
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        let m = menu.as_mut().unwrap();
                        m.selected = (m.selected + n - 1) % n;
                        continue;
                    }
                    KeyCode::Enter => {
                        accept_completion(&mut prompt, &menu.take().unwrap());
                        continue;
                    }
                    KeyCode::Esc => {
                        menu = None;
                        continue;
                    }
                    KeyCode::Char('c') if ctrl => {
                        menu = None;
                        continue;
                    }
                    _ => menu = None, // dismiss, then edit normally below
                }
            }

            match k.code {
                KeyCode::Char('c') if ctrl => {
                    if prompt.is_empty() {
                        break Composed::Done(Read::Interrupt);
                    }
                    prompt.clear();
                }
                KeyCode::Char('d') if ctrl => {
                    if prompt.is_empty() {
                        break Composed::Done(Read::Eof);
                    }
                    // A non-empty buffer: Ctrl-D deletes the char under the
                    // cursor (readline's behaviour), not EOF.
                    prompt.handle_key(k);
                }
                // Up/Down walk history only from the prompt's edge rows: with
                // the cursor mid-text in a multi-line draft they fall through
                // to the editor and move the cursor instead.
                KeyCode::Up if k.modifiers.is_empty() => {
                    if prompt.row() == 0 {
                        self.history_prev(&mut prompt, &mut hist_pos, &mut draft);
                    } else {
                        prompt.handle_key(k);
                    }
                }
                KeyCode::Down if k.modifiers.is_empty() => {
                    if prompt.row() == prompt.row_count() - 1 {
                        self.history_next(&mut prompt, &mut hist_pos, &mut draft);
                    } else {
                        prompt.handle_key(k);
                    }
                }
                KeyCode::Tab => {
                    // Build the candidate snapshot once, then complete the
                    // token under the cursor.  A unique match is applied in
                    // place; several open the menu; none is a no-op.
                    let row = prompt.row();
                    let col = prompt.col();
                    let Some(line) = prompt.line(row) else {
                        prompt.handle_key(k);
                        continue;
                    };
                    let src = sources.get_or_insert_with(|| Sources::from_shell(shell));
                    let cursor_byte = char_to_byte(&line, col);
                    let (start, candidates) = completion::complete(&line, cursor_byte, src);
                    match candidates.as_slice() {
                        [] => {}
                        [only] => {
                            prompt.replace_row_bytes(row, start, cursor_byte, &only.replacement);
                        }
                        _ => {
                            #[allow(
                                clippy::cast_possible_truncation,
                                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
                            )]
                            let anchor_col =
                                prompt_lines.last_w + line[..start].chars().count() as u16;
                            menu = Some(Menu {
                                candidates,
                                selected: 0,
                                replace_from: start,
                                row,
                                anchor_col,
                            });
                        }
                    }
                }
                // Right-arrow at end-of-buffer accepts the autosuggestion
                // ghost (parity with rustyline's hint-accept); elsewhere it
                // moves the cursor.
                KeyCode::Right if k.modifiers.is_empty() => match &ghost {
                    Some(g) if !g.is_empty() && prompt.at_buffer_end() => {
                        prompt.insert_str(g);
                    }
                    _ => {
                        prompt.handle_key(k);
                    }
                },
                KeyCode::Enter => {
                    let line = prompt.text();
                    if ral_core::syntax::parser::needs_continuation(&line) {
                        prompt.insert_str("\n");
                    } else {
                        break Composed::Done(Read::Line(line));
                    }
                }
                _ => {
                    // A registered plugin chord breaks to dispatch outside the
                    // viewport; anything else is ordinary editing.  Built-in
                    // editing keys above take precedence, so a plugin cannot
                    // shadow Ctrl-C/Ctrl-D/history/Tab/Enter.
                    let chords =
                        chords.get_or_insert_with(|| lock(&self.runtime).keybinding_chords());
                    let hit = chords.iter().find_map(|(name, bi, chord)| {
                        key_matches(&k, chord).then(|| (name.clone(), *bi))
                    });
                    if let Some((plugin, binding_idx)) = hit {
                        break Composed::Keybinding(PendingKeybinding {
                            plugin,
                            binding_idx,
                            cursor_byte: prompt.cursor_byte_offset(),
                        });
                    }
                    prompt.handle_key(k);
                }
            }
        };

        match &result {
            // Commit the entered command into scrollback above the viewport
            // (so it reads like an ordinary shell): the styled prompt prefix
            // followed by the submitted text, in the live prompt's colours.
            // `insert_before` leaves the now-cleared viewport right below the
            // committed line.
            Composed::Done(Read::Line(text)) => commit_line(&mut terminal, &prompt_lines, text)?,
            // Nothing to commit (Interrupt / Eof / re-edit), or a plugin key
            // about to dispatch: clear the viewport so the next prompt — or a
            // keybinding handler taking the terminal — starts on a clean band.
            _ => terminal.clear()?,
        }
        // Park the cursor at the viewport's top-left origin before leaving raw
        // mode.  Otherwise it sits wherever the last draw left it — mid-band —
        // and the command's output starts there, with a blank gap above it and
        // the committed line scrolled off the top.  Setting it here makes the
        // output flow from directly under the committed line, nothing lost.
        let origin = terminal.get_frame().area();
        terminal.set_cursor_position(Position::new(origin.x, origin.y))?;
        disable_raw_mode()?;
        // Release the inline viewport before any handler runs: a plugin
        // keybinding's `_ed-tui` (fzf, zoxide) takes over the terminal.
        drop(terminal);

        let read = match result {
            Composed::Done(read) => read,
            // The chord fired mid-compose; run its handler now that raw mode
            // is off and the viewport is gone — the same dispatch the rustyline
            // frontend runs once `readline` returns.  Accept yields a ready
            // line; Edit re-feeds the buffer (fzf-files / fzf-history land
            // here, the edited buffer reappearing on the next read).
            Composed::Keybinding(pk) => {
                let buf = prompt.text();
                match dispatch_keybinding(pk, &buf, shell, &self.runtime, keymap) {
                    KeybindingOutcome::Accept(line) => Read::Line(line),
                    KeybindingOutcome::Edit(text, cursor) => {
                        Read::Edit(EditBuffer { text, cursor })
                    }
                }
            }
        };
        // Flush plugin diagnostics deferred during composition or dispatch on
        // every exit path, so they land on a durable line above the next prompt.
        flush_pending_messages(&self.runtime);
        Ok(read)
    }

    /// Recall the previous history entry (Up from the first row).  The live
    /// draft is stashed on entry and navigation clamps at the oldest entry;
    /// a no-op when history is empty.
    fn history_prev(&self, prompt: &mut PromptEditor, pos: &mut Option<usize>, draft: &mut String) {
        let entries = self.history.entries();
        if entries.is_empty() {
            return;
        }
        let next = match *pos {
            None => {
                *draft = prompt.text();
                entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        *pos = Some(next);
        prompt.set_text(&entries[next]);
    }

    /// Recall the next history entry (Down from the last row), or restore the
    /// stashed draft once browsing walks past the newest entry.  A no-op when
    /// not browsing history.
    fn history_next(&self, prompt: &mut PromptEditor, pos: &mut Option<usize>, draft: &mut String) {
        match *pos {
            None => {}
            Some(i) if i + 1 < self.history.entries().len() => {
                *pos = Some(i + 1);
                let entry = self.history.entries()[i + 1].clone();
                prompt.set_text(&entry);
            }
            Some(_) => {
                // Past the newest entry: restore the in-progress draft.
                *pos = None;
                let draft = std::mem::take(draft);
                prompt.set_text(&draft);
            }
        }
    }
}

impl Frontend for StructuralFrontend {
    fn read(
        &mut self,
        shell: &mut Shell,
        prompt: &PromptText,
        pending: Option<EditBuffer>,
        #[cfg(unix)] jobs: &Arc<Mutex<JobTable>>,
        #[cfg(feature = "structural")] worksheet: &Worksheet,
    ) -> Read {
        if let Ok(r) = self.compose(
            shell,
            prompt,
            pending,
            #[cfg(unix)]
            jobs,
            worksheet,
        ) {
            r
        } else {
            // A terminal IO failure mid-session: leave raw mode and end
            // cleanly rather than spin.
            let _ = disable_raw_mode();
            Read::Eof
        }
    }

    fn add_history(&mut self, entry: &str) {
        self.history.add(entry);
    }

    fn save_history(&mut self) {
        self.history.save();
    }
}

// ── The editor ──────────────────────────────────────────────────────────────

/// A fresh editor: flat styling, no cursor-line underline (matching the
/// surrounding chrome), and a plain cursor style — the widget's painted cursor
/// cell is suppressed so the prompt shows the terminal's own (native, blinking)
/// cursor instead, positioned each frame by [`render_prompt`].  The native
/// cursor is the same in every mode; there is no painted vi modal-mode block.
/// Whether the cursor sits at the very end of the buffer (last row, last
/// column) — the position fish-style autosuggestion is accepted from.
/// The cursor's absolute byte offset into the `\n`-joined buffer — the unit the
/// buffer-change hooks and [`dispatch_keybinding`] expect (both convert to
/// chars internally).  Sums each prior row's bytes plus its newline, then the
/// cursor's byte column within its row.
/// Whether a crossterm key event matches a frontend-neutral plugin [`KeyChord`].
/// Ctrl/Alt must match exactly; Shift is ignored, as no bindable chord carries
/// it.
fn key_matches(k: &KeyEvent, chord: &KeyChord) -> bool {
    if k.modifiers.contains(KeyModifiers::CONTROL) != chord.ctrl
        || k.modifiers.contains(KeyModifiers::ALT) != chord.alt
    {
        return false;
    }
    match (chord.name, k.code) {
        (KeyName::Char(a), KeyCode::Char(b)) => a == b,
        (KeyName::Tab, KeyCode::Tab)
        | (KeyName::Enter, KeyCode::Enter)
        | (KeyName::Escape, KeyCode::Esc)
        | (KeyName::Up, KeyCode::Up)
        | (KeyName::Down, KeyCode::Down)
        | (KeyName::Left, KeyCode::Left)
        | (KeyName::Right, KeyCode::Right)
        | (KeyName::Home, KeyCode::Home)
        | (KeyName::End, KeyCode::End)
        | (KeyName::Delete, KeyCode::Delete)
        | (KeyName::Backspace, KeyCode::Backspace) => true,
        (KeyName::F(a), KeyCode::F(b)) => a == b,
        _ => false,
    }
}

// Replace the editor contents, leaving the cursor at the end — the unit of
// every history recall and draft restore.
// Move the cursor to a character offset into the (just-filled) buffer,
// clamped to its length — restores an [`EditBuffer`] saved cursor as
// closely as the row/col editor allows.
// ── Completion ───────────────────────────────────────────────────────────────

/// Apply the selected candidate of an open [`Menu`]: replace the token from
/// the menu's `replace_from` to the current cursor (which has not moved while
/// the menu owned the keys) with the chosen replacement.  Aborts if the cursor
/// has left the trigger row.
fn accept_completion(prompt: &mut PromptEditor, menu: &Menu) {
    let row = prompt.row();
    if row != menu.row {
        return;
    }
    let Some(line) = prompt.line(row) else {
        return;
    };
    let end = char_to_byte(&line, prompt.col());
    let replacement = menu.candidates[menu.selected].replacement.clone();
    prompt.replace_row_bytes(row, menu.replace_from, end, &replacement);
}
// ── Projection 1: the typed spine ───────────────────────────────────────────

/// One pipeline stage's row in the spine: its source slice and value type.
#[derive(Clone, PartialEq)]
struct SpineRow {
    src: String,
    ty: String,
}

/// What the spine shows for the current buffer.
#[derive(Clone, PartialEq)]
enum Spine {
    /// A pipeline: one typed row per stage.
    Stages(Vec<SpineRow>),
    /// A type error — underlined in place on the prompt, ariadne-style.
    /// `span` is a half-open CHAR range into the prompt buffer (the same
    /// coordinate system the editor's char cursor uses); `None` when the
    /// error carries no location, in which case only the dim headline shows.
    TypeError {
        span: Option<(usize, usize)>,
        code: String,
        headline: String,
        label: String,
        hint: Option<String>,
    },
    /// Nothing to show: an empty or still-incomplete buffer, or a buffer
    /// that compiles but is not a pipeline.
    Empty,
}

/// Re-infer the spine for the buffer against the live session.
fn build_spine(src: &str, shell: &Shell) -> Spine {
    if src.trim().is_empty() {
        return Spine::Empty;
    }
    match ral_core::compile_and_typecheck(
        src,
        shell.session_schemes(),
        ral_core::source::FileId::DUMMY,
    ) {
        CompileOutcome::Compiled(comp) => match pipeline_stage_rows(&comp, src) {
            Some(rows) => Spine::Stages(rows),
            None => Spine::Empty,
        },
        // A parse error mid-typing is an incomplete line, not a real error:
        // show nothing rather than flare on every keystroke.
        CompileOutcome::Parse(_) => Spine::Empty,
        CompileOutcome::Types(errs) => match errs.first() {
            // Reuse core's diagnostic phrasing verbatim — the headline, the
            // under-caret label, and the code are exactly what the post-Enter
            // ariadne report uses, so the two agree word for word.
            Some(err) => Spine::TypeError {
                span: err.pos.map(|sp| {
                    (
                        ral_core::diagnostic::byte_to_char(src, sp.start as usize),
                        ral_core::diagnostic::byte_to_char(src, sp.end as usize),
                    )
                }),
                code: err.kind.code().to_string(),
                headline: err.kind.render_message(),
                label: ral_core::diagnostic::label_message_for_kind(&err.kind),
                hint: err.hint(),
            },
            None => Spine::Empty,
        },
    }
}

/// The first top-level pipeline in `comp`, if any — the line being composed
/// elaborates to a bare `Pipeline`, or one under a `let` bind or a sequence.
fn find_pipeline(comp: &Comp) -> Option<&Comp> {
    match &comp.item {
        CompKind::Pipeline { .. } => Some(comp),
        CompKind::Seq(parts) => parts.iter().find_map(|p| find_pipeline(p)),
        CompKind::Bind {
            comp: bound, rest, ..
        } => find_pipeline(bound).or_else(|| find_pipeline(rest)),
        _ => None,
    }
}

/// Per-stage rows for the first pipeline in `comp`, each pairing the stage's
/// source slice with its retained value type.
fn pipeline_stage_rows(comp: &Comp, src: &str) -> Option<Vec<SpineRow>> {
    let pipe = find_pipeline(comp)?;
    let CompKind::Pipeline {
        stages,
        stage_types,
        ..
    } = &pipe.item
    else {
        return None;
    };
    let rows = stages
        .iter()
        .zip(stage_types)
        .map(|(stage, ty)| SpineRow {
            src: stage
                .span
                .and_then(|sp| src.get(sp.start as usize..sp.end as usize))
                .unwrap_or("")
                .trim()
                .to_string(),
            ty: fmt_ty(ty),
        })
        .collect();
    Some(rows)
}

// ── Projections 2 & 3: worksheet and handles matrix ─────────────────────────

/// One worksheet node: a user binding with its type and value preview, its
/// nesting `depth` in the dependency tree, and its pure/effectful verdict.
///
/// `name`/`ty`/`preview` come from the live env each `read`; `depth` and
/// `effectful` come from the session's [`Worksheet`] model (the retained
/// dependency edges and the checker's effect verdict).  A node with no model
/// entry — a binding that predates the model, or whose record was dropped —
/// renders as a depth-0, pure node, so the projection never goes blank on
/// missing edge data.
struct WsRow {
    name: String,
    ty: String,
    preview: String,
    depth: usize,
    effectful: bool,
}

/// One matrix row's lifecycle state, unifying the two kinds of live work the
/// matrix projects: env-held [`Value::Handle`] spawns (a [`HandleState`]) and
/// pgid jobs parked or resumed by the kernel (a [`crate::jobs::JobState`],
/// Unix only).  Sharing one enum keeps a single render loop and glyph map.
#[derive(Clone, Copy)]
enum MxState {
    Running,
    Completed,
    Cancelled,
    Stopped,
}

impl From<HandleState> for MxState {
    fn from(s: HandleState) -> Self {
        match s {
            HandleState::Running => Self::Running,
            HandleState::Completed => Self::Completed,
            HandleState::Cancelled => Self::Cancelled,
        }
    }
}

#[cfg(unix)]
impl From<crate::jobs::JobState> for MxState {
    fn from(s: crate::jobs::JobState) -> Self {
        match s {
            crate::jobs::JobState::Running => Self::Running,
            crate::jobs::JobState::Stopped => Self::Stopped,
        }
    }
}

/// One matrix row: a unit of live work — an env-held spawn handle or a pgid
/// job — with its lifecycle state and a label.
struct MxRow {
    name: String,
    state: MxState,
    cmd: String,
}

/// Every binding name currently in scope — the baseline snapshot.
fn binding_names(shell: &Shell) -> HashSet<String> {
    shell.bindings().into_iter().map(|(n, _)| n).collect()
}

/// The user's bindings — those added since the baseline — as a single
/// snapshot the worksheet and matrix projections share.
fn user_bindings(shell: &Shell, baseline: &HashSet<String>) -> Vec<(String, Value)> {
    shell
        .bindings()
        .into_iter()
        .filter(|(n, _)| !baseline.contains(n))
        .collect()
}

/// The user's bindings rendered as worksheet rows, laid out as an indented
/// dependency tree: a binding nests under the binding it depends on, so
/// dependents read downstream of what feeds them.
///
/// Name, type, and value preview come from the live env (`user` +
/// `binding_schemes`); the dependency edges and the pure/effectful verdict
/// come from the session's [`Worksheet`] model.  The two are joined by name:
/// only bindings present in the *live env* are nodes (a model entry whose
/// binding is gone is skipped); the model supplies each present node's depth
/// and effect glyph.
///
/// The dependency relation is a DAG, but the render is a tree, so each node
/// hangs under one chosen parent — the *latest-recorded* of its dependencies
/// that is itself a live node — which keeps the indentation reading as the
/// data-flow chain.  A node with no live-binding dependency is a root.
fn worksheet_rows(user: &[(String, Value)], shell: &Shell, model: &Worksheet) -> Vec<WsRow> {
    let schemes: std::collections::HashMap<String, Option<Scheme>> =
        shell.binding_schemes().into_iter().collect();
    let live: HashSet<&str> = user.iter().map(|(n, _)| n.as_str()).collect();

    // Record order indexes the model entries; a node's "latest dependency"
    // is the one with the greatest record index, so the tree nests along the
    // direction definitions were added.
    let order: std::collections::HashMap<&str, usize> = model
        .entries()
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.as_str(), i))
        .collect();

    // Per live node: its model entry's effect verdict, and its chosen parent
    // (the latest-recorded live dependency, or none → root).  Children are
    // grouped under each parent, roots under the `None` key.
    let mut effectful: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    let mut children: std::collections::HashMap<Option<&str>, Vec<&str>> =
        std::collections::HashMap::new();
    let mut rooted: Vec<&str> = Vec::new();

    for (name, _) in user {
        let name = name.as_str();
        let entry = model.entries().iter().find(|e| e.name == name);
        effectful.insert(name, entry.is_some_and(|e| e.effectful));
        // The parent is the live dependency recorded latest; a node may
        // depend on several, but the tree picks one to hang it under.
        let parent = entry.and_then(|e| {
            e.free_refs
                .iter()
                .map(String::as_str)
                .filter(|d| live.contains(d) && *d != name)
                .max_by_key(|d| order.get(d).copied().unwrap_or(0))
        });
        match parent {
            Some(p) => children.entry(Some(p)).or_default().push(name),
            None => rooted.push(name),
        }
    }

    // Order roots and each child group by record index, then alphabetically,
    // for a stable layout.
    let sort_key = |n: &&str| (order.get(*n).copied().unwrap_or(usize::MAX), n.to_string());
    rooted.sort_by_key(sort_key);
    for kids in children.values_mut() {
        kids.sort_by_key(sort_key);
    }

    // Preorder DFS over the forest.  Seed the worklist with the roots (in
    // order), then sweep any node the root walk missed — a dependency cycle
    // (mutually recursive lambdas) leaves every member with a parent and so
    // out of `rooted`, but it must still render.  A visited set guards the
    // cycle so the walk always terminates.  A stack `(name, depth)` gives the
    // depth a node inherits from its parent.
    let mut rows = Vec::with_capacity(user.len());
    let mut visited: HashSet<&str> = HashSet::new();
    let seeds = rooted
        .iter()
        .copied()
        .chain(user.iter().map(|(n, _)| n.as_str()));
    for seed in seeds {
        if visited.contains(seed) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(seed, 0)];
        while let Some((name, depth)) = stack.pop() {
            if !visited.insert(name) {
                continue;
            }
            let value = user
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v)
                .expect("a tree node is a live user binding");
            let ty = schemes
                .get(name)
                .and_then(|s| s.as_ref())
                .map_or_else(|| "?".into(), fmt_scheme);
            rows.push(WsRow {
                name: name.to_string(),
                ty,
                preview: preview(value),
                depth,
                effectful: effectful.get(name).copied().unwrap_or(false),
            });
            if let Some(kids) = children.get(&Some(name)) {
                for kid in kids.iter().rev() {
                    stack.push((*kid, depth + 1));
                }
            }
        }
    }
    rows
}

/// The matrix rows: the user's live env-held spawn handles followed by the
/// session's pgid jobs (`job_rows`, empty off Unix).  Handles sort by binding
/// name; jobs follow in job-id order, the order `jobs`/`fg`/`bg` use.
fn matrix_rows(user: &[(String, Value)], mut job_rows: Vec<MxRow>) -> Vec<MxRow> {
    let mut rows: Vec<MxRow> = user
        .iter()
        .filter_map(|(name, value)| match value {
            Value::Handle(h) => Some(MxRow {
                name: name.clone(),
                state: (*h.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)).into(),
                cmd: h.cmd.clone(),
            }),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.append(&mut job_rows);
    rows
}

/// The session's pgid jobs as matrix rows, in job-id order.  Locks the table
/// briefly, copies each job's id / command / state into an owned row, and
/// drops the guard before returning so nothing is held across rendering.
/// Labelled `%id` after the shell's job-spec syntax.
#[cfg(unix)]
fn job_rows(jobs: &Arc<Mutex<JobTable>>) -> Vec<MxRow> {
    let guard = jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .list()
        .into_iter()
        .map(|j| MxRow {
            name: format!("%{}", j.id),
            state: j.state.into(),
            cmd: j.cmd.clone(),
        })
        .collect()
}

/// A one-line value preview, truncated.
fn preview(v: &Value) -> String {
    const CAP: usize = 40;
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() > CAP {
        let mut t: String = s.chars().take(CAP - 1).collect();
        t.push('…');
        t
    } else {
        s
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// A prompt split into its rendered rows.  A prompt may carry newlines
/// (`\n`): every line but the last is a *lead* row drawn above the editor,
/// and the last line is the inline prefix the editor's first row sits beside
/// — mirroring how a terminal lays a multi-line prompt out.  A plain
/// single-line prompt has an empty `lead` and is entirely its `last` line.
struct PromptLines {
    /// Lines before the last, drawn as standalone rows above the editor.
    lead: Vec<Line<'static>>,
    /// The final prompt line: the inline prefix the editor begins after.
    last: Line<'static>,
    /// Display width of `last` — the column the editor's first row begins at.
    last_w: u16,
}

/// Parse the styled prompt into ratatui spans, split on its newlines, falling
/// back to the ANSI-stripped raw text when the escapes do not parse — never to
/// nothing.  ansi-to-tui carries SGR state across the newlines, so a colour
/// opened before a `\n` still styles the line after it.
fn split_prompt(prompt: &PromptText) -> PromptLines {
    let text = prompt
        .styled()
        .into_text()
        .unwrap_or_else(|_| Text::from(prompt.raw().to_string()));
    let mut lines = text.lines;
    let last = lines.pop().unwrap_or_default();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let last_w = last.width() as u16;
    PromptLines {
        lead: lines,
        last,
        last_w,
    }
}

/// The editor's own visible row count: one row per logical line.  (The
/// `WrapMode::None` editor does not soft-wrap, so logical lines are rows.)
fn prompt_rows(prompt: &PromptEditor) -> u16 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let rows = prompt.row_count().max(1) as u16;
    rows
}

/// Size the inline viewport to its content at entry — spine, prompt, and the
/// projections' header-plus-rows — clamped to hug the bottom of the screen.
fn viewport_height(
    lead: u16,
    prompt: &PromptEditor,
    worksheet: &[WsRow],
    matrix: &[MxRow],
    rows: u16,
) -> u16 {
    // The spine is empty at entry (inference runs only inside the loop), so
    // the content is the prompt's lead rows plus its editor rows, plus the
    // taller projection column.  Each column shows a header row plus one row
    // per entry — an empty column still shows its header and a placeholder
    // row.  One extra row is reserved for the caret/label row of a type error,
    // which can flare on any keystroke: the viewport is sized once per read,
    // so it must afford that row up front rather than steal it from the
    // projections when the error appears.
    let prompt = lead + prompt_rows(prompt);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let ws = 1 + worksheet.len().max(1) as u16;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let mx = 1 + matrix.len().max(1) as u16;
    // The lower band holds either the projections or a completion menu,
    // whichever is taller, so a menu opened on a fresh session (empty
    // worksheet) still has room to drop down rather than being clipped to the
    // two-row projection placeholder.
    let lower = ws.max(mx).max(MENU_MAX_ROWS + 2);
    let needed = prompt + 1 + lower;
    needed
        .clamp(1, MAX_VIEWPORT)
        .min(rows.saturating_sub(1).max(1))
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    prompt: &PromptLines,
    editor: &mut PromptEditor,
    spine: &Spine,
    worksheet: &[WsRow],
    matrix: &[MxRow],
    menu: Option<&Menu>,
    ghost: Option<&str>,
    highlights: &[HighlightSpan],
) {
    let area = frame.area();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let lead_rows = prompt.lead.len() as u16;
    let editor_rows = prompt_rows(editor);

    // A type error replaces the per-stage rows above the prompt with a single
    // caret/label row beneath it; the stage spine keeps its rows above and
    // claims no row below.
    let (spine_rows, caret_rows) = match spine {
        Spine::Stages(rows) => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let n = rows.len() as u16;
            (n, 0)
        }
        Spine::TypeError { .. } => (0, 1),
        Spine::Empty => (0, 0),
    };

    let [spine_area, prompt_area, caret_area, rest] = Layout::vertical([
        Constraint::Length(spine_rows),
        Constraint::Length(lead_rows + editor_rows),
        Constraint::Length(caret_rows),
        Constraint::Min(0),
    ])
    .areas(area);

    // Within the prompt block: a multi-line prompt's lead rows above, then the
    // editable band where the last prompt line is the inline prefix the editor
    // sits beside.
    let [lead_area, editor_band] = Layout::vertical([
        Constraint::Length(lead_rows),
        Constraint::Length(editor_rows),
    ])
    .areas(prompt_area);

    render_spine(frame, spine_area, spine);
    render_prompt(frame, lead_area, editor_band, prompt, editor);
    // The plugin overlays and the type-error underline all read the cells the
    // TextArea just painted, so they run after `render_prompt`.  Order matters
    // on conflict: highlights first, ghost (past the typed text) next, then the
    // type-error flare last so it wins any cell a highlight also claimed.
    overlay_highlights(frame, editor_band, prompt.last_w, editor, highlights);
    overlay_ghost(frame, editor_band, prompt.last_w, editor, ghost);
    overlay_type_error(frame, editor_band, caret_area, prompt.last_w, editor, spine);
    render_projections(frame, rest, worksheet, matrix);
    // The completion menu drops down over the top of the projection band,
    // anchored under the token being completed; it owns the keys while open,
    // so the projections beneath it are inert and may be covered.
    if let Some(m) = menu {
        render_menu(frame, rest, m);
    }
}

fn render_spine(frame: &mut ratatui::Frame, area: Rect, spine: &Spine) {
    if area.height == 0 {
        return;
    }
    let lines: Vec<Line> = match spine {
        Spine::Stages(rows) => rows
            .iter()
            .map(|r| {
                Line::from(vec![
                    Span::styled("│ ", Style::default().fg(SLATE)),
                    Span::styled(format!("{} ", r.src), Style::default().fg(NAME_HUE)),
                    Span::styled(": ", Style::default().fg(SLATE)),
                    Span::styled(r.ty.clone(), Style::default().fg(TYPE_HUE)),
                ])
            })
            .collect(),
        // The type error draws below the prompt, not here.
        Spine::TypeError { .. } | Spine::Empty => return,
    };
    frame.render_widget(Paragraph::new(lines), area);
}

/// Underline the offending span in place on the prompt (ariadne's squiggle),
/// then draw a caret-and-label row directly beneath it, aligned under the
/// span.  Char offsets, not bytes — they match the `TextArea`'s char cursor.
///
/// The underline is overlaid onto the cells the `TextArea` already painted:
/// for each char of the span on the prompt's first row, we add
/// [`Modifier::UNDERLINED`] and the flare hue to the existing cell rather
/// than fighting the widget's styling API.  The common one-line prompt is
/// handled precisely; a span that escapes the first row (multi-line buffer)
/// degrades to the caret/label row alone.
fn overlay_type_error(
    frame: &mut ratatui::Frame,
    editor_area: Rect,
    caret_area: Rect,
    last_w: u16,
    editor: &PromptEditor,
    spine: &Spine,
) {
    let Spine::TypeError {
        span,
        code,
        headline,
        label,
        ..
    } = spine
    else {
        return;
    };
    if caret_area.height == 0 {
        return;
    }

    // The editor text begins `last_w` columns into the editable band's first
    // row.  In the common single-line buffer the span's char offsets map
    // straight onto columns past the prefix; a span starting beyond the first
    // row belongs to a continuation line we do not underline.
    let first_row_len = editor.lines().first().map_or(0, |l| l.chars().count());

    match span {
        // A located error on the (single-line) prompt: underline it in place
        // and point a caret row at it.
        Some((start, end)) if *start <= first_row_len => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let span_start_col = *start as u16;
            // Clamp the span's end to the first row — a span running past the
            // editor's first line still gets a sensible underline/caret.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let span_end_col = (*end).min(first_row_len) as u16;
            let span_w = span_end_col.saturating_sub(span_start_col).max(1);

            underline_cells(frame, editor_area, last_w + span_start_col, span_w);
            render_caret_row(
                frame,
                caret_area,
                last_w + span_start_col,
                span_w,
                label,
                code,
            );
        }
        // No span (or it escaped the first row): no underline, just the dim
        // headline beneath the prompt — the messageless fallback.
        _ => {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("✗ {headline}"),
                    Style::default().fg(FLARE_HUE).add_modifier(Modifier::DIM),
                ))),
                caret_area,
            );
        }
    }
}

/// Overlay the flare hue and an underline onto `span_w` already-painted cells
/// of the prompt row, starting at column `col` within `area`.
fn underline_cells(frame: &mut ratatui::Frame, area: Rect, col: u16, span_w: u16) {
    let buf = frame.buffer_mut();
    let y = area.y;
    let x0 = area.x + col;
    let max_x = area.x + area.width;
    let flare = Style::default()
        .fg(FLARE_HUE)
        .add_modifier(Modifier::UNDERLINED);
    for x in x0..(x0 + span_w).min(max_x) {
        buf[(x, y)].set_style(flare);
    }
}

/// Paint the autosuggestion ghost dim, starting at the cursor's screen cell
/// and running right along its row, onto the cells the `TextArea` already
/// painted.  Same coordinate model as [`overlay_type_error`]: every editor row
/// begins `last_w` columns into the band, including continuation rows.  The
/// cursor sits over the first ghost cell — fish-style — and Right-arrow accepts
/// it (see [`StructuralFrontend::compose`]).
fn overlay_ghost(
    frame: &mut ratatui::Frame,
    band: Rect,
    last_w: u16,
    editor: &PromptEditor,
    ghost: Option<&str>,
) {
    let Some(ghost) = ghost.filter(|g| !g.is_empty()) else {
        return;
    };
    if band.height == 0 {
        return;
    }
    let row = editor.row();
    let col = editor.col();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let y = band.y + row as u16;
    let max_x = band.x + band.width;
    if y >= band.y + band.height {
        return;
    }
    let dim = Style::default().fg(SLATE).add_modifier(Modifier::DIM);
    let buf = frame.buffer_mut();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let x0 = band.x + last_w + col as u16;
    for (x, ch) in (x0..).zip(ghost.chars()) {
        if x >= max_x {
            break;
        }
        let cell = &mut buf[(x, y)];
        cell.set_char(ch);
        cell.set_style(dim);
    }
}

/// Paint plugin highlight spans onto the prompt's editor cells.  Each span is
/// a half-open CHAR range into the `\n`-joined buffer (the same units the
/// `_ed-*` surface speaks); [`abs_char_to_row_col`] maps each offset to its
/// editor row/column, and [`style_ratatui`] gives the cell style.  Same
/// cell-overlay technique as [`underline_cells`]; an unknown style name or an
/// offset on a newline / past the text is skipped.
fn overlay_highlights(
    frame: &mut ratatui::Frame,
    band: Rect,
    last_w: u16,
    editor: &PromptEditor,
    highlights: &[HighlightSpan],
) {
    if highlights.is_empty() || band.height == 0 {
        return;
    }
    let lines = editor.lines();
    let max_x = band.x + band.width;
    let max_y = band.y + band.height;
    let buf = frame.buffer_mut();
    for hs in highlights {
        let Some(style) = style_ratatui(&hs.style) else {
            continue;
        };
        for abs in hs.span.range() {
            let Some((row, col)) = abs_char_to_row_col(&lines, abs) else {
                continue;
            };
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let x = band.x + last_w + col as u16;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let y = band.y + row as u16;
            if x < max_x && y < max_y {
                buf[(x, y)].set_style(style);
            }
        }
    }
}

/// Map an absolute character offset into the `\n`-joined buffer to its
/// `(row, col)` within the editor's lines.  Returns `None` when the offset
/// lands on a row's terminating newline or runs past the end — neither has a
/// visible cell to style.
fn abs_char_to_row_col(lines: &[String], abs: usize) -> Option<(usize, usize)> {
    let mut remaining = abs;
    for (row, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if remaining < len {
            return Some((row, remaining));
        }
        // Consume this row's chars plus its newline; landing exactly on the
        // newline (or past the last row) yields `None`.
        remaining = remaining.checked_sub(len + 1)?;
    }
    None
}

/// Draw the caret-and-label row: pad to the span's start column, lay down
/// `^` under each span char in the flare hue, then the under-caret label and
/// the dim error code.
fn render_caret_row(
    frame: &mut ratatui::Frame,
    area: Rect,
    col: u16,
    span_w: u16,
    label: &str,
    code: &str,
) {
    let line = Line::from(vec![
        Span::raw(" ".repeat(col as usize)),
        Span::styled("^".repeat(span_w as usize), Style::default().fg(FLARE_HUE)),
        Span::styled(format!(" {label}"), Style::default().fg(FLARE_HUE)),
        Span::styled(format!(" [{code}]"), Style::default().fg(SLATE)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render a (possibly multi-line) prompt: its lead rows fill `lead_area`
/// above, then the last prompt line is the inline prefix at column 0 of the
/// editable band's first row, with the editor in a sub-area offset to its
/// right.  The terminal's own native cursor is positioned at the edit point —
/// the editor sub-area's origin already includes the prompt prefix, so it is
/// just that origin plus the editor's row/col.
fn render_prompt(
    frame: &mut ratatui::Frame,
    lead_area: Rect,
    editor_band: Rect,
    prompt: &PromptLines,
    editor: &mut PromptEditor,
) {
    if lead_area.height > 0 {
        frame.render_widget(Paragraph::new(prompt.lead.clone()), lead_area);
    }
    if editor_band.height == 0 {
        return;
    }
    // The last prompt line occupies only the first row of the band; render it
    // there, then the editor in the sub-area offset to its right.
    let prefix_area = Rect {
        height: 1,
        ..editor_band
    };
    frame.render_widget(Paragraph::new(prompt.last.clone()), prefix_area);

    let editor_area = Rect {
        x: editor_band.x + prompt.last_w,
        width: editor_band.width.saturating_sub(prompt.last_w),
        ..editor_band
    };
    editor.render(frame, editor_area);
    // No block on this editor, so the render area is the text rect.  The native
    // cursor shows in every mode; the widget's painted cell is suppressed (set
    // to a plain style in `new_textarea`).
    if let Some(pos) = editor.cursor_screen_position() {
        let x = editor_area.x + pos.x.min(editor_area.width.saturating_sub(1));
        let y = editor_area.y + pos.y.min(editor_area.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(x, y));
    }
}

fn render_projections(
    frame: &mut ratatui::Frame,
    area: Rect,
    worksheet: &[WsRow],
    matrix: &[MxRow],
) {
    if area.height == 0 {
        return;
    }
    let [ws_area, mx_area] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);

    // Worksheet
    let mut ws_lines = vec![Line::from(Span::styled(
        "worksheet",
        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
    ))];
    if worksheet.is_empty() {
        ws_lines.push(Line::from(Span::styled(
            "  (no bindings)",
            Style::default().fg(SLATE),
        )));
    } else {
        for r in worksheet
            .iter()
            .take(area.height.saturating_sub(1) as usize)
        {
            // Indent by depth so dependents sit under what they depend on; a
            // distinct glyph and hue mark effectful nodes (which would not
            // re-flow freely) apart from pure ones.
            let indent = "  ".repeat(r.depth);
            let (glyph, hue) = if r.effectful {
                ("◆", EFFECT_HUE)
            } else {
                ("●", NAME_HUE)
            };
            ws_lines.push(Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{glyph} "), Style::default().fg(hue)),
                Span::styled(r.name.clone(), Style::default().fg(NAME_HUE)),
                Span::styled(" : ", Style::default().fg(SLATE)),
                Span::styled(r.ty.clone(), Style::default().fg(TYPE_HUE)),
                Span::styled(format!(" = {}", r.preview), Style::default().fg(SLATE)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(ws_lines), ws_area);

    // Handles matrix
    let mut mx_lines = vec![Line::from(Span::styled(
        "handles",
        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
    ))];
    if matrix.is_empty() {
        mx_lines.push(Line::from(Span::styled("  —", Style::default().fg(SLATE))));
    } else {
        for r in matrix
            .iter()
            .take(mx_area.height.saturating_sub(1) as usize)
        {
            let (glyph, hue) = match r.state {
                MxState::Running => ("●", HANDLE_RUN),
                MxState::Completed => ("✓", NAME_HUE),
                MxState::Cancelled | MxState::Stopped => ("○", SLATE),
            };
            mx_lines.push(Line::from(vec![
                Span::styled(format!("{glyph} {}", r.name), Style::default().fg(hue)),
                Span::styled(format!("  {}", r.cmd), Style::default().fg(SLATE)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(mx_lines), mx_area);
}

/// Draw the completion menu as a bordered popup dropping down over the top of
/// the projection band, its left edge anchored under the token being completed
/// (clamped to stay within the band).  The selected row is reversed; the list
/// scrolls within [`MENU_MAX_ROWS`] so a long candidate set stays navigable.
fn render_menu(frame: &mut ratatui::Frame, area: Rect, menu: &Menu) {
    if area.height < 3 || menu.candidates.is_empty() {
        return;
    }
    // Width fits the widest candidate plus borders; height fits the visible
    // rows plus borders.  Both clamp to the band.
    let widest = menu
        .candidates
        .iter()
        .map(|c| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
            )]
            let w = c.display.chars().count() as u16;
            w
        })
        .max()
        .unwrap_or(0);
    let pop_w = (widest + 2).clamp(10, area.width);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let visible = (menu.candidates.len() as u16)
        .min(MENU_MAX_ROWS)
        .min(area.height - 2);
    let rect = Rect {
        x: area.x + menu.anchor_col.min(area.width.saturating_sub(pop_w)),
        y: area.y,
        width: pop_w,
        height: visible + 2,
    };

    // Scroll the window so the selected row stays visible.
    let window = visible as usize;
    let start = menu.selected.saturating_sub(window.saturating_sub(1));
    let lines: Vec<Line> = menu
        .candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(window)
        .map(|(i, c)| {
            let mut style = Style::default().fg(NAME_HUE);
            if i == menu.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(Span::styled(c.display.clone(), style))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(SLATE));
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Commit the submitted line into scrollback above the viewport: the styled
/// prompt — its lead rows and last-line prefix — followed by the entered
/// text, in the live prompt's colours.
fn commit_line(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    prompt: &PromptLines,
    text: &str,
) -> io::Result<()> {
    // Replay the full prompt — its lead rows, then the last line's spans
    // prepended to the command's first line — so the committed scrollback
    // reads exactly like the live prompt did.
    let mut lines: Vec<Line> = prompt.lead.clone();
    for (i, line) in text.split('\n').enumerate() {
        if i == 0 {
            let mut spans = prompt.last.spans.clone();
            spans.push(Span::raw(line.to_string()));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
    )]
    let height = lines.len().max(1) as u16;
    terminal.insert_before(height, |b| {
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .render(b.area, b);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer the checker rejects with a located error builds the
    /// `TypeError` spine: a char span to underline and the exact under-caret
    /// label core's diagnostic uses — so the inline flare and the post-Enter
    /// report agree word for word.  `if "x" { … }` mismatches the condition's
    /// `String` against the expected `Bool`, pointing at the `"x"` literal.
    #[test]
    fn build_spine_locates_type_error() {
        let shell = Shell::new(ral_core::io::TerminalState::default());
        let spine = build_spine("if \"x\" { 1 } else { 2 }", &shell);
        let Spine::TypeError {
            span, label, code, ..
        } = spine
        else {
            panic!("a located type error yields the TypeError spine");
        };
        // The span points at the `"x"` condition (chars 3..6), to underline.
        assert_eq!(span, Some((3, 6)));
        assert!(!label.is_empty(), "carries an under-caret label");
        assert_eq!(label, "String doesn't match Bool");
        assert_eq!(code, "T0010");
    }

    /// The spine extracts a typed row per pipeline stage from the retained
    /// per-stage value types — the keystone projection, end to end.
    #[test]
    fn spine_rows_carry_per_stage_types() {
        let outcome = ral_core::compile_and_typecheck(
            "/bin/echo hi | /bin/cat",
            ral_core::typecheck::SessionSchemes::default(),
            ral_core::source::FileId::DUMMY,
        );
        let CompileOutcome::Compiled(comp) = outcome else {
            panic!("pipeline should compile");
        };
        let rows = pipeline_stage_rows(&comp, "/bin/echo hi | /bin/cat")
            .expect("a pipeline yields per-stage rows");
        assert_eq!(rows.len(), 2, "two stages");
        assert_eq!(rows[0].src, "/bin/echo hi");
        assert_eq!(rows[1].src, "/bin/cat");
        // Both external stages resolve to `String` (the retained value type).
        assert_eq!(rows[0].ty, "String");
        assert_eq!(rows[1].ty, "String");
    }

    /// A non-pipeline buffer yields no spine rows.
    #[test]
    fn non_pipeline_has_no_stage_rows() {
        let outcome = ral_core::compile_and_typecheck(
            "/bin/echo hi",
            ral_core::typecheck::SessionSchemes::default(),
            ral_core::source::FileId::DUMMY,
        );
        let CompileOutcome::Compiled(comp) = outcome else {
            panic!("should compile");
        };
        assert!(pipeline_stage_rows(&comp, "/bin/echo hi").is_none());
    }

    /// A long value preview is truncated with an ellipsis.
    #[test]
    fn preview_truncates() {
        let v = Value::String("x".repeat(100));
        let p = preview(&v);
        assert!(p.chars().count() <= 40);
        assert!(p.ends_with('…'));
    }

    /// The worksheet rows nest dependents under what they depend on: with
    /// `b = $a` and `c = $b`, the tree reads `a` (depth 0) ▸ `b` (1) ▸ `c`
    /// (2), so the indentation traces the data-flow chain.  Names/values come
    /// from the `user` list (the live env stand-in); edges from the model.
    #[test]
    fn worksheet_rows_nest_dependents_under_dependencies() {
        let shell = Shell::new(ral_core::io::TerminalState::default());
        let mut model = Worksheet::default();
        model.record("let a = 1", &shell);
        model.record("let b = $a", &shell);
        model.record("let c = $b", &shell);
        let user = vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(1)),
            ("c".to_string(), Value::Int(1)),
        ];
        let rows = worksheet_rows(&user, &shell, &model);
        let shape: Vec<(&str, usize)> = rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(shape, vec![("a", 0), ("b", 1), ("c", 2)]);
    }

    /// An effectful binding (an external command) carries the effect flag on
    /// its row; a pure one does not — the marker the render distinguishes.
    #[test]
    fn worksheet_rows_carry_the_effect_verdict() {
        let shell = Shell::new(ral_core::io::TerminalState::default());
        let mut model = Worksheet::default();
        model.record("let n = $[1 + 2]", &shell);
        model.record("let p = /bin/echo hi", &shell);
        let user = vec![
            ("n".to_string(), Value::Int(3)),
            ("p".to_string(), Value::Unit),
        ];
        let rows = worksheet_rows(&user, &shell, &model);
        let n = rows.iter().find(|r| r.name == "n").unwrap();
        let p = rows.iter().find(|r| r.name == "p").unwrap();
        assert!(!n.effectful, "arithmetic is pure");
        assert!(p.effectful, "an external command is effectful");
        // Both are roots: `p` has no dependency on `n`.
        assert_eq!(n.depth, 0);
        assert_eq!(p.depth, 0);
    }

    /// A live binding with no model entry (it predates the model) still
    /// renders — as a depth-0, pure node — so missing edge data never blanks
    /// the projection.
    #[test]
    fn worksheet_row_without_a_model_entry_renders_as_a_pure_root() {
        let shell = Shell::new(ral_core::io::TerminalState::default());
        let model = Worksheet::default();
        let user = vec![("legacy".to_string(), Value::Int(7))];
        let rows = worksheet_rows(&user, &shell, &model);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "legacy");
        assert_eq!(rows[0].depth, 0);
        assert!(!rows[0].effectful);
    }

    /// Absolute char offsets map onto editor `(row, col)`; an offset that lands
    /// on a row's newline or runs past the text has no visible cell → `None`.
    #[test]
    fn abs_char_to_row_col_skips_newlines_and_overflow() {
        let lines: Vec<String> = vec!["ab".into(), "cd".into()];
        assert_eq!(abs_char_to_row_col(&lines, 0), Some((0, 0)));
        assert_eq!(abs_char_to_row_col(&lines, 1), Some((0, 1)));
        assert_eq!(abs_char_to_row_col(&lines, 2), None); // the newline after "ab"
        assert_eq!(abs_char_to_row_col(&lines, 3), Some((1, 0)));
        assert_eq!(abs_char_to_row_col(&lines, 4), Some((1, 1)));
        assert_eq!(abs_char_to_row_col(&lines, 5), None); // past the end
    }

    /// A crossterm key matches a plugin chord only when its name and ctrl/alt
    /// modifiers agree; shift is ignored, a wrong char or modifier misses.
    #[test]
    fn key_matches_compares_name_and_modifiers() {
        let ctrl_r = KeyChord {
            name: KeyName::Char('r'),
            ctrl: true,
            alt: false,
        };
        let ev = |code, mods| KeyEvent::new(code, mods);
        assert!(key_matches(
            &ev(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &ctrl_r
        ));
        // Wrong char, a missing modifier, or an extra alt all miss.
        assert!(!key_matches(
            &ev(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &ctrl_r
        ));
        assert!(!key_matches(
            &ev(KeyCode::Char('r'), KeyModifiers::NONE),
            &ctrl_r
        ));
        assert!(!key_matches(
            &ev(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            &ctrl_r
        ));
        // Shift alongside ctrl is tolerated — no bindable chord carries shift.
        assert!(key_matches(
            &ev(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &ctrl_r
        ));
    }
}
