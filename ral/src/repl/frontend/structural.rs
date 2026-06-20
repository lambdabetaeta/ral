//! The structural frontend (`RAL_SURFACE=structural`).
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
//! read-only worksheet, and an env-held handles matrix.  The reactive
//! re-flow, fork, and pgid-job rows the design proposal also describes are
//! later parcels; the projections here read runtime state and never mutate
//! it.

use ral_core::Shell;
use ral_core::ir::{Comp, CompKind};
use ral_core::typecheck::{Scheme, fmt_scheme, fmt_ty};
use ral_core::types::HandleState;
use ral_core::{CompileOutcome, Value};

use ansi_to_tui::IntoText;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};
use ratatui_textarea::{CursorMove, TextArea};

use std::collections::HashSet;
use std::io;
use std::time::Duration;

use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, History, Read};

// ── Palette ───────────────────────────────────────────────────────────────

const TYPE_HUE: Color = Color::Rgb(135, 200, 215); // cyan — types
const NAME_HUE: Color = Color::Rgb(165, 210, 155); // lime — binding names
const FLARE_HUE: Color = Color::Rgb(215, 110, 125); // red — type-error flare
const SLATE: Color = Color::Rgb(140, 150, 170); // dim chrome
const HANDLE_RUN: Color = Color::Rgb(220, 140, 175); // pink — running handle

/// Idle redraw cadence: the structural surface polls keys and redraws the
/// (constant, between-keystroke) projections at this interval.
const TICK: Duration = Duration::from_millis(120);

/// Upper bound on the inline viewport's height, so a session with many
/// bindings never swallows the whole screen — scrollback must stay visible.
const MAX_VIEWPORT: u16 = 18;

pub(in crate::repl) struct StructuralFrontend {
    history: History,
    /// The set of binding names present at the first `read` — the prelude
    /// and prompt bindings — so the worksheet shows only what the user has
    /// since defined.  Captured lazily because `new` has no shell.
    baseline: Option<HashSet<String>>,
}

impl StructuralFrontend {
    /// Construct the frontend, verifying the terminal supports raw mode (so
    /// the boot selector can fall back when it does not).  Loads persisted
    /// history like the other frontends.
    pub(in crate::repl) fn new() -> io::Result<Self> {
        // Probe raw mode once: if the terminal cannot do it, the structural
        // surface cannot run and the caller degrades to a line editor.
        enable_raw_mode()?;
        disable_raw_mode()?;
        Ok(Self {
            history: History::load(),
            baseline: None,
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
    ) -> io::Result<Read> {
        // The worksheet and matrix read the env, which does not change while
        // the user composes (no evaluation happens here), so build them once.
        // Both project the same user bindings, so fold the scope once and
        // derive both from that single snapshot.
        let baseline = self.baseline.get_or_insert_with(|| binding_names(shell));
        let user = user_bindings(shell, baseline);
        let worksheet = worksheet_rows(&user, shell);
        let matrix = matrix_rows(&user);

        // The styled prompt prefix, parsed into spans once: ansi-to-tui turns
        // the SGR escapes into ratatui styling.  A parse failure degrades to
        // the ANSI-stripped raw text — never to a blank prompt.
        let prefix = prompt_prefix(prompt);
        let prefix_w = prompt.raw().chars().count() as u16;

        let mut textarea = new_textarea();
        if let Some(p) = &pending {
            textarea.insert_str(&p.text);
            place_cursor(&mut textarea, p.cursor);
        }
        let mut hist_pos: Option<usize> = None;
        let mut draft = String::new();

        enable_raw_mode()?;
        let (_cols, rows) = size().unwrap_or((80, 24));
        // Size the viewport to its content at entry: the spine, the prompt's
        // own (possibly multi-line) rows, and the projections.  Clamping to
        // `rows - 1` hugs the bottom — the prompt sits where a shell prompt
        // always sits — and `MAX_VIEWPORT` keeps scrollback in view.
        let height = viewport_height(&textarea, &worksheet, &matrix, rows);
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
            let s = textarea.lines().join("\n");
            if last_buf.as_deref() != Some(s.as_str()) {
                spine = build_spine(&s, shell);
                last_buf = Some(s.clone());
            }
            terminal.draw(|frame| {
                render(frame, &prefix, prefix_w, &textarea, &spine, &worksheet, &matrix);
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
            match k.code {
                KeyCode::Char('c') if ctrl => {
                    if is_empty(&textarea) {
                        break Read::Interrupt;
                    }
                    textarea.select_all();
                    textarea.cut();
                }
                KeyCode::Char('d') if ctrl => {
                    if is_empty(&textarea) {
                        break Read::Eof;
                    }
                }
                // Up/Down walk history only from the prompt's edge rows: with
                // the cursor mid-text in a multi-line draft they fall through
                // to the textarea and move the cursor instead.
                KeyCode::Up if k.modifiers.is_empty() => {
                    if textarea.cursor().0 == 0 {
                        self.history_prev(&mut textarea, &mut hist_pos, &mut draft);
                    } else {
                        textarea.input(k);
                    }
                }
                KeyCode::Down if k.modifiers.is_empty() => {
                    if textarea.cursor().0 == textarea.lines().len() - 1 {
                        self.history_next(&mut textarea, &mut hist_pos, &mut draft);
                    } else {
                        textarea.input(k);
                    }
                }
                KeyCode::Enter => {
                    let line = textarea.lines().join("\n");
                    if ral_core::syntax::parser::needs_continuation(&line) {
                        textarea.insert_newline();
                    } else {
                        break Read::Line(line);
                    }
                }
                _ => {
                    textarea.input(k);
                }
            }
        };

        match &result {
            // Commit the entered command into scrollback above the viewport
            // (so it reads like an ordinary shell): the styled prompt prefix
            // followed by the submitted text, in the live prompt's colours.
            // `insert_before` leaves the now-cleared viewport right below the
            // committed line.
            Read::Line(text) => commit_line(&mut terminal, &prefix, text)?,
            // No line to commit (Interrupt / Eof): just clear the viewport so
            // the next prompt — or the shell's exit — starts on a clean band.
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
        Ok(result)
    }

    /// Recall the previous history entry (Up from the first row).  The live
    /// draft is stashed on entry and navigation clamps at the oldest entry;
    /// a no-op when history is empty.
    fn history_prev(&self, textarea: &mut TextArea<'static>, pos: &mut Option<usize>, draft: &mut String) {
        let entries = self.history.entries();
        if entries.is_empty() {
            return;
        }
        let next = match *pos {
            None => {
                *draft = textarea.lines().join("\n");
                entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        *pos = Some(next);
        set_text(textarea, &entries[next]);
    }

    /// Recall the next history entry (Down from the last row), or restore the
    /// stashed draft once browsing walks past the newest entry.  A no-op when
    /// not browsing history.
    fn history_next(&self, textarea: &mut TextArea<'static>, pos: &mut Option<usize>, draft: &mut String) {
        match *pos {
            None => {}
            Some(i) if i + 1 < self.history.entries().len() => {
                *pos = Some(i + 1);
                let entry = self.history.entries()[i + 1].clone();
                set_text(textarea, &entry);
            }
            Some(_) => {
                // Past the newest entry: restore the in-progress draft.
                *pos = None;
                let draft = std::mem::take(draft);
                set_text(textarea, &draft);
            }
        }
    }
}

impl Frontend for StructuralFrontend {
    fn read(&mut self, shell: &mut Shell, prompt: &PromptText, pending: Option<EditBuffer>) -> Read {
        match self.compose(shell, prompt, pending) {
            Ok(r) => r,
            Err(_) => {
                // A terminal IO failure mid-session: leave raw mode and end
                // cleanly rather than spin.
                let _ = disable_raw_mode();
                Read::Eof
            }
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
/// surrounding chrome), so the prompt reads as one continuous line of text.
fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

/// Whether the editor holds no text at all (every line empty).
fn is_empty(ta: &TextArea<'static>) -> bool {
    ta.lines().iter().all(|l| l.is_empty())
}

/// Replace the editor's contents, leaving the cursor at the end — the unit of
/// every history recall and draft restore.
fn set_text(ta: &mut TextArea<'static>, s: &str) {
    ta.select_all();
    ta.cut();
    ta.insert_str(s);
}

/// Move the cursor to a character offset into the (just-filled) buffer,
/// clamped to its length — restores an [`EditBuffer`]'s saved cursor as
/// closely as the row/col editor allows.
fn place_cursor(ta: &mut TextArea<'static>, char_offset: usize) {
    ta.move_cursor(CursorMove::Top);
    ta.move_cursor(CursorMove::Head);
    for _ in 0..char_offset {
        ta.move_cursor(CursorMove::Forward);
    }
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
    /// A type error — the flare.
    Flare(String),
    /// Nothing to show: an empty or still-incomplete buffer, or a buffer
    /// that compiles but is not a pipeline.
    Empty,
}

/// Re-infer the spine for the buffer against the live session.
fn build_spine(src: &str, shell: &Shell) -> Spine {
    if src.trim().is_empty() {
        return Spine::Empty;
    }
    match ral_core::compile_and_typecheck(src, shell.session_schemes()) {
        CompileOutcome::Compiled(comp) => match pipeline_stage_rows(&comp, src) {
            Some(rows) => Spine::Stages(rows),
            None => Spine::Empty,
        },
        // A parse error mid-typing is an incomplete line, not a real error:
        // show nothing rather than flare on every keystroke.
        CompileOutcome::Parse(_) => Spine::Empty,
        CompileOutcome::Types(errs) => Spine::Flare(
            errs.first()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "type error".into()),
        ),
    }
}

/// The first top-level pipeline in `comp`, if any — the line being composed
/// elaborates to a bare `Pipeline`, or one under a `let` bind or a sequence.
fn find_pipeline(comp: &Comp) -> Option<&Comp> {
    match &comp.item {
        CompKind::Pipeline { .. } => Some(comp),
        CompKind::Seq(parts) => parts.iter().find_map(|p| find_pipeline(p)),
        CompKind::Bind { comp: bound, rest, .. } => {
            find_pipeline(bound).or_else(|| find_pipeline(rest))
        }
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

/// One worksheet node: a user binding with its type and value preview.
struct WsRow {
    name: String,
    ty: String,
    preview: String,
}

/// One matrix row: a live env-held handle.
struct MxRow {
    name: String,
    state: HandleState,
    cmd: String,
}

/// Every binding name currently in scope — the baseline snapshot.
fn binding_names(shell: &Shell) -> HashSet<String> {
    shell
        .mobile
        .scope
        .all_bindings()
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// The user's bindings — those added since the baseline — as a single
/// snapshot the worksheet and matrix projections share.
fn user_bindings(shell: &Shell, baseline: &HashSet<String>) -> Vec<(String, Value)> {
    shell
        .mobile
        .scope
        .all_bindings()
        .into_iter()
        .filter(|(n, _)| !baseline.contains(n))
        .collect()
}

/// The user's bindings rendered as worksheet rows, each with its scheme.
fn worksheet_rows(user: &[(String, Value)], shell: &Shell) -> Vec<WsRow> {
    let schemes: std::collections::HashMap<String, Option<Scheme>> =
        shell.mobile.scope.binding_schemes().into_iter().collect();
    let mut rows: Vec<WsRow> = user
        .iter()
        .map(|(name, value)| {
            let ty = schemes
                .get(name)
                .and_then(|s| s.as_ref())
                .map(fmt_scheme)
                .unwrap_or_else(|| "?".into());
            WsRow {
                ty,
                preview: preview(value),
                name: name.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// The user's live spawn handles, as matrix rows.
fn matrix_rows(user: &[(String, Value)]) -> Vec<MxRow> {
    let mut rows: Vec<MxRow> = user
        .iter()
        .filter_map(|(name, value)| match value {
            Value::Handle(h) => Some(MxRow {
                name: name.clone(),
                state: *h.state.lock().unwrap_or_else(|e| e.into_inner()),
                cmd: h.cmd.clone(),
            }),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
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

/// Parse the styled prompt into ratatui spans, falling back to the
/// ANSI-stripped raw text when the escapes do not parse — never to nothing.
fn prompt_prefix(prompt: &PromptText) -> Line<'static> {
    match prompt.styled().into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .next()
            .unwrap_or_else(|| Line::from(Span::raw(prompt.raw().to_string()))),
        Err(_) => Line::from(Span::raw(prompt.raw().to_string())),
    }
}

/// The editor's own visible row count: one row per logical line.  (The
/// `WrapMode::None` editor does not soft-wrap, so logical lines are rows.)
fn prompt_rows(textarea: &TextArea<'static>) -> u16 {
    textarea.lines().len().max(1) as u16
}

/// Size the inline viewport to its content at entry — spine, prompt, and the
/// projections' header-plus-rows — clamped to hug the bottom of the screen.
fn viewport_height(
    textarea: &TextArea<'static>,
    worksheet: &[WsRow],
    matrix: &[MxRow],
    rows: u16,
) -> u16 {
    // The spine is empty at entry (inference runs only inside the loop), so
    // the content is the prompt rows plus the taller projection column.  Each
    // column shows a header row plus one row per entry — an empty column still
    // shows its header and a placeholder row.
    let prompt = prompt_rows(textarea);
    let ws = 1 + worksheet.len().max(1) as u16;
    let mx = 1 + matrix.len().max(1) as u16;
    let needed = prompt + ws.max(mx);
    needed.clamp(1, MAX_VIEWPORT).min(rows.saturating_sub(1).max(1))
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    prefix: &Line<'static>,
    prefix_w: u16,
    textarea: &TextArea<'static>,
    spine: &Spine,
    worksheet: &[WsRow],
    matrix: &[MxRow],
) {
    let area = frame.area();
    let prompt_lines = prompt_rows(textarea);

    let spine_rows = match spine {
        Spine::Stages(rows) => rows.len() as u16,
        Spine::Empty => 0,
        Spine::Flare(_) => 1,
    };

    let [spine_area, prompt_area, rest] = Layout::vertical([
        Constraint::Length(spine_rows),
        Constraint::Length(prompt_lines),
        Constraint::Min(0),
    ])
    .areas(area);

    render_spine(frame, spine_area, spine);
    render_prompt(frame, prompt_area, prefix, prefix_w, textarea);
    render_projections(frame, rest, worksheet, matrix);
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
        Spine::Flare(msg) => vec![Line::from(Span::styled(
            format!("✗ {msg}"),
            Style::default().fg(FLARE_HUE),
        ))],
        Spine::Empty => return,
    };
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the styled prompt prefix at column 0 of the prompt row, then the
/// editor in a sub-area offset to its right.  The textarea paints its own
/// cursor into that sub-area, so the cursor lands correctly past the prefix;
/// continuation rows indent under the editable text, which reads fine.
fn render_prompt(
    frame: &mut ratatui::Frame,
    area: Rect,
    prefix: &Line<'static>,
    prefix_w: u16,
    textarea: &TextArea<'static>,
) {
    if area.height == 0 {
        return;
    }
    // The prefix occupies only the first row; render it there.
    let prefix_area = Rect { height: 1, ..area };
    frame.render_widget(Paragraph::new(prefix.clone()), prefix_area);

    let editor_area = Rect {
        x: area.x + prefix_w,
        width: area.width.saturating_sub(prefix_w),
        ..area
    };
    frame.render_widget(textarea, editor_area);
}

fn render_projections(frame: &mut ratatui::Frame, area: Rect, worksheet: &[WsRow], matrix: &[MxRow]) {
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
        for r in worksheet.iter().take(area.height.saturating_sub(1) as usize) {
            ws_lines.push(Line::from(vec![
                Span::styled(format!("● {}", r.name), Style::default().fg(NAME_HUE)),
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
        for r in matrix.iter().take(mx_area.height.saturating_sub(1) as usize) {
            let (glyph, hue) = match r.state {
                HandleState::Running => ("●", HANDLE_RUN),
                HandleState::Completed => ("✓", NAME_HUE),
                HandleState::Cancelled => ("○", SLATE),
            };
            mx_lines.push(Line::from(vec![
                Span::styled(format!("{glyph} {}", r.name), Style::default().fg(hue)),
                Span::styled(format!("  {}", r.cmd), Style::default().fg(SLATE)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(mx_lines), mx_area);
}

/// Commit the submitted line into scrollback above the viewport: the styled
/// prompt prefix followed by the entered text, in the live prompt's colours.
fn commit_line(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    prefix: &Line<'static>,
    text: &str,
) -> io::Result<()> {
    // Prepend the styled prefix spans to the command's first line so the
    // committed scrollback line reads exactly like the live prompt did.
    let mut lines: Vec<Line> = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if i == 0 {
            let mut spans = prefix.spans.clone();
            spans.push(Span::raw(line.to_string()));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
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

    /// The spine extracts a typed row per pipeline stage from the retained
    /// per-stage value types — the keystone projection, end to end.
    #[test]
    fn spine_rows_carry_per_stage_types() {
        let outcome = ral_core::compile_and_typecheck(
            "/bin/echo hi | /bin/cat",
            ral_core::typecheck::SessionSchemes::default(),
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
        );
        let CompileOutcome::Compiled(comp) = outcome else {
            panic!("should compile");
        };
        assert!(pipeline_stage_rows(&comp, "/bin/echo hi").is_none());
    }

    /// Pre-filling the editor from an [`EditBuffer`] restores the text and
    /// lands the cursor at the saved char offset — across a newline, the
    /// row/col cursor sits on the right row and column.
    #[test]
    fn place_cursor_restores_offset_across_newlines() {
        let mut ta = new_textarea();
        ta.insert_str("ab\ncd");
        place_cursor(&mut ta, 0);
        assert_eq!(ta.cursor(), (0, 0));
        place_cursor(&mut ta, 2);
        assert_eq!(ta.cursor(), (0, 2)); // before the newline
        place_cursor(&mut ta, 3);
        assert_eq!(ta.cursor(), (1, 0)); // the newline counts as one forward step
        place_cursor(&mut ta, 4);
        assert_eq!(ta.cursor(), (1, 1));
        // An over-range offset clamps at the end rather than panicking.
        place_cursor(&mut ta, 99);
        assert_eq!(ta.cursor(), (1, 2));
    }

    /// Replacing the editor's contents (history recall / draft restore)
    /// swaps the text wholesale and parks the cursor at the end.
    #[test]
    fn set_text_replaces_contents() {
        let mut ta = new_textarea();
        ta.insert_str("first draft");
        set_text(&mut ta, "recalled entry");
        assert_eq!(ta.lines(), ["recalled entry"]);
        assert_eq!(ta.cursor(), (0, "recalled entry".chars().count()));
        // A multi-line recall round-trips its newlines.
        set_text(&mut ta, "a\nb");
        assert_eq!(ta.lines(), ["a", "b"]);
    }

    /// A long value preview is truncated with an ellipsis.
    #[test]
    fn preview_truncates() {
        let v = Value::String("x".repeat(100));
        let p = preview(&v);
        assert!(p.chars().count() <= 40);
        assert!(p.ends_with('…'));
    }
}
