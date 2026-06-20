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

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};

use std::collections::HashSet;
use std::io;
use std::time::Duration;

use super::super::config::dirs_history;
use super::super::prompt::PromptText;
use super::{EditBuffer, Frontend, Read};

// ── Palette ───────────────────────────────────────────────────────────────

const TYPE_HUE: Color = Color::Rgb(135, 200, 215); // cyan — types
const NAME_HUE: Color = Color::Rgb(165, 210, 155); // lime — binding names
const FLARE_HUE: Color = Color::Rgb(215, 110, 125); // red — type-error flare
const SLATE: Color = Color::Rgb(140, 150, 170); // dim chrome
const HANDLE_RUN: Color = Color::Rgb(220, 140, 175); // pink — running handle

/// Idle redraw cadence: the structural surface polls keys and redraws the
/// (constant, between-keystroke) projections at this interval.
const TICK: Duration = Duration::from_millis(120);

pub(in crate::repl) struct StructuralFrontend {
    history: Vec<String>,
    persisted: usize,
    history_path: Option<String>,
    /// The set of binding names present at the first `read` — the prelude
    /// and prompt bindings — so the worksheet shows only what the user has
    /// since defined.  Captured lazily because `new` has no shell.
    baseline: Option<HashSet<String>>,
}

impl StructuralFrontend {
    /// Construct the frontend, verifying the terminal supports raw mode (so
    /// the boot selector can fall back when it does not).  Loads persisted
    /// history like the other frontends.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:history-read] loads persisted repl history at construction; not turn-time model I/O"
    )]
    pub(in crate::repl) fn new() -> io::Result<Self> {
        // Probe raw mode once: if the terminal cannot do it, the structural
        // surface cannot run and the caller degrades to a line editor.
        enable_raw_mode()?;
        disable_raw_mode()?;
        let history_path = dirs_history();
        let history: Vec<String> = history_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default();
        let persisted = history.len();
        Ok(Self {
            history,
            persisted,
            history_path,
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
        let baseline = self.baseline.get_or_insert_with(|| binding_names(shell));
        let worksheet = worksheet_rows(shell, baseline);
        let matrix = matrix_rows(shell, baseline);

        enable_raw_mode()?;
        let (_cols, rows) = size().unwrap_or((80, 24));
        let height = (rows / 2).clamp(8, 18);
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        let mut buf: Vec<char> = pending
            .as_ref()
            .map(|p| p.text.chars().collect())
            .unwrap_or_default();
        let mut cursor = pending.as_ref().map_or(buf.len(), |p| p.cursor.min(buf.len()));
        let mut hist_pos: Option<usize> = None;
        let mut draft: Vec<char> = Vec::new();

        // The spine is recomputed only when the buffer changes; an idle
        // redraw reuses the cached one.
        let mut spine = Spine::Empty;
        let mut last_buf: Option<String> = None;

        let result = loop {
            let s: String = buf.iter().collect();
            if last_buf.as_deref() != Some(s.as_str()) {
                spine = build_spine(&s, shell);
                last_buf = Some(s.clone());
            }
            terminal.draw(|frame| {
                render(frame, prompt, &buf, cursor, &spine, &worksheet, &matrix);
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
                    if buf.is_empty() {
                        break Read::Interrupt;
                    }
                    buf.clear();
                    cursor = 0;
                }
                KeyCode::Char('d') if ctrl => {
                    if buf.is_empty() {
                        break Read::Eof;
                    }
                }
                KeyCode::Char('u') if ctrl => {
                    buf.drain(..cursor);
                    cursor = 0;
                }
                KeyCode::Char('a') if ctrl => cursor = 0,
                KeyCode::Char('e') if ctrl => cursor = buf.len(),
                KeyCode::Char(c) => {
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        buf.remove(cursor - 1);
                        cursor -= 1;
                    }
                }
                KeyCode::Delete => {
                    if cursor < buf.len() {
                        buf.remove(cursor);
                    }
                }
                KeyCode::Left => cursor = cursor.saturating_sub(1),
                KeyCode::Right => cursor = (cursor + 1).min(buf.len()),
                KeyCode::Home => cursor = 0,
                KeyCode::End => cursor = buf.len(),
                KeyCode::Up => self.history_prev(&mut buf, &mut cursor, &mut hist_pos, &mut draft),
                KeyCode::Down => self.history_next(&mut buf, &mut cursor, &mut hist_pos, &mut draft),
                KeyCode::Enter => {
                    let line: String = buf.iter().collect();
                    if ral_core::syntax::parser::needs_continuation(&line) {
                        buf.insert(cursor, '\n');
                        cursor += 1;
                    } else {
                        break Read::Line(line);
                    }
                }
                _ => {}
            }
        };

        // Commit the entered command into scrollback above the viewport (so
        // it reads like an ordinary shell), then clear the viewport and hand
        // the terminal back for the command's own output.
        if let Read::Line(text) = &result {
            let committed = format!("{}{}", prompt.raw(), text);
            let lines = committed.lines().count().max(1) as u16;
            let _ = terminal.insert_before(lines, |b| {
                Paragraph::new(committed.clone())
                    .wrap(Wrap { trim: false })
                    .render(b.area, b);
            });
        }
        terminal.clear()?;
        disable_raw_mode()?;
        Ok(result)
    }

    fn history_prev(
        &self,
        buf: &mut Vec<char>,
        cursor: &mut usize,
        pos: &mut Option<usize>,
        draft: &mut Vec<char>,
    ) {
        if self.history.is_empty() {
            return;
        }
        let next = match *pos {
            None => {
                *draft = buf.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        *pos = Some(next);
        *buf = self.history[next].chars().collect();
        *cursor = buf.len();
    }

    fn history_next(
        &self,
        buf: &mut Vec<char>,
        cursor: &mut usize,
        pos: &mut Option<usize>,
        draft: &mut Vec<char>,
    ) {
        match *pos {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                *pos = Some(i + 1);
                *buf = self.history[i + 1].chars().collect();
                *cursor = buf.len();
            }
            Some(_) => {
                // Past the newest entry: restore the in-progress draft.
                *pos = None;
                *buf = std::mem::take(draft);
                *cursor = buf.len();
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
        if self.history.last().is_none_or(|s| s != entry) {
            self.history.push(entry.to_string());
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:history-append] appends this session's repl history to its log file; not turn-time model I/O"
    )]
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
    /// The buffer compiles but is not a pipeline.
    Ok,
    /// A type error — the flare.
    Flare(String),
    /// Nothing to show (empty or still-incomplete buffer).
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
            None => Spine::Ok,
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

/// The user's bindings (those added since the baseline), as worksheet rows.
fn worksheet_rows(shell: &Shell, baseline: &HashSet<String>) -> Vec<WsRow> {
    let schemes: std::collections::HashMap<String, Option<Scheme>> =
        shell.mobile.scope.binding_schemes().into_iter().collect();
    let mut rows: Vec<WsRow> = shell
        .mobile
        .scope
        .all_bindings()
        .into_iter()
        .filter(|(n, _)| !baseline.contains(n))
        .map(|(name, value)| {
            let ty = schemes
                .get(&name)
                .and_then(|s| s.as_ref())
                .map(fmt_scheme)
                .unwrap_or_else(|| "?".into());
            WsRow {
                ty,
                preview: preview(&value),
                name,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// The user's live spawn handles, as matrix rows.
fn matrix_rows(shell: &Shell, baseline: &HashSet<String>) -> Vec<MxRow> {
    let mut rows: Vec<MxRow> = shell
        .mobile
        .scope
        .all_bindings()
        .into_iter()
        .filter(|(n, _)| !baseline.contains(n))
        .filter_map(|(name, value)| match value {
            Value::Handle(h) => Some(MxRow {
                name,
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

/// Map a character index in `buf` to a (row, column) within the rendered
/// prompt, accounting for the prompt prefix on the first line.
fn cursor_rc(buf: &[char], cursor: usize, prefix_w: u16) -> (u16, u16) {
    let mut row = 0u16;
    let mut col = prefix_w;
    for &c in &buf[..cursor.min(buf.len())] {
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    prompt: &PromptText,
    buf: &[char],
    cursor: usize,
    spine: &Spine,
    worksheet: &[WsRow],
    matrix: &[MxRow],
) {
    let area = frame.area();
    let buf_string: String = buf.iter().collect();
    let prompt_lines = buf_string.lines().count().max(1) as u16;

    let spine_rows = match spine {
        Spine::Stages(rows) => rows.len() as u16,
        Spine::Ok | Spine::Empty => 0,
        Spine::Flare(_) => 1,
    };

    let [spine_area, prompt_area, rest] = Layout::vertical([
        Constraint::Length(spine_rows),
        Constraint::Length(prompt_lines),
        Constraint::Min(0),
    ])
    .areas(area);

    render_spine(frame, spine_area, spine);
    render_prompt(frame, prompt_area, prompt, buf, &buf_string, cursor);
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
        Spine::Ok | Spine::Empty => return,
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_prompt(
    frame: &mut ratatui::Frame,
    area: Rect,
    prompt: &PromptText,
    buf: &[char],
    buf_string: &str,
    cursor: usize,
) {
    let prefix = prompt.raw();
    let prefix_w = prefix.chars().count() as u16;
    let mut lines: Vec<Line> = Vec::new();
    for (i, line) in buf_string.split('\n').enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), Style::default().fg(SLATE)),
                Span::raw(line.to_string()),
            ]));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
    let (r, c) = cursor_rc(buf, cursor, prefix_w);
    frame.set_cursor_position((area.x + c, area.y + r));
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

    /// The cursor maps past a newline onto the next rendered row at column 0.
    #[test]
    fn cursor_rc_tracks_newlines() {
        let buf: Vec<char> = "ab\ncd".chars().collect();
        // Prefix width 2 ("❯ "), cursor at index 4 (the 'd').
        assert_eq!(cursor_rc(&buf, 0, 2), (0, 2));
        assert_eq!(cursor_rc(&buf, 2, 2), (0, 4)); // before the newline
        assert_eq!(cursor_rc(&buf, 3, 2), (1, 0)); // after the newline
        assert_eq!(cursor_rc(&buf, 4, 2), (1, 1));
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
