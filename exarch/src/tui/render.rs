//! Render loop: frame drawing and terminal output.
//!
//! The free function [`draw`] paints the whole frame — content area,
//! queued-user strip, tab bar / matrix, prompt editor, status line, and
//! footer — into a [`Term`].  The helper functions paint selection and hover
//! highlights into the line buffer before it reaches the terminal.

use std::collections::HashMap;
use std::io::{self, Write};
use std::str;

use crossterm::{
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, size},
};

use ratatui::{
    crossterm::event::MouseEvent,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::bus::AgentId;

use super::App;
use super::app::Overlay;
use super::block::queued_prompt_rows;
use super::gesture::COPY_TOAST_TTL;
use super::line;
use super::palette::{AGENT_HUES, LIME_HOT, PINK, READ_W, SLATE};
use super::matrix::matrix_bar;
use super::select::highlight_range;
use super::status::{StatusReadout, rule_line};
use super::terminal::Term;

const PROMPT_PAD_H: u16 = 1;

/// Left gutter for the transcript, queued-prompt strip, and rule line.
/// Gives the marginal rail breathing room from the terminal edge so it
/// reads as a Bertin data column rather than frame chrome.
const LEFT_MARGIN: u16 = 2;
/// Minimum useful width of the pinned-state register column, in columns.  Once
/// the content area has this much space to the right of the `READ_W`-capped
/// transcript, the register takes all of it.
const REGISTER_MIN_W: u16 = 35;

/// Braille spinner glyphs for the terminal tab title, rotated 4 ticks per frame (~15 fps).
const SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Where the content area sat in the last drawn frame.
#[derive(Clone, Copy)]
pub(super) struct FrameGeom {
    pub(super) text: Rect,
    /// First visible buffer row, mapping screen rows to buffer rows.
    pub(super) offset: usize,
}
impl FrameGeom {
    /// Map a mouse event's screen cell to buffer `(row, col)` — the scrolled
    /// buffer row and the cell column within the text area (0 = left edge) —
    /// or `None` when the event lands outside the content area.
    pub(super) fn buffer_coords(&self, me: MouseEvent) -> Option<(usize, u16)> {
        contains(self.text, me.column, me.row).then(|| {
            let row = self.offset + (me.row - self.text.y) as usize;
            (row, me.column - self.text.x)
        })
    }
}
/// Whether the cell `(col, row)` lies inside `rect`.
pub(super) fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}
/// Paint one frame into `term`.
pub(super) fn draw(app: &mut App, term: &mut Term) -> io::Result<()> {
    let (cols, rows) = size().unwrap_or((READ_W, 24));
    let area = Rect::new(0, 0, cols, rows);
    // The prompt box sizes to its draft; the `/model` picker floats as an
    // overlay above this whole layout (drawn last over a cleared centre),
    // so it no longer claims the prompt region.
    let text_w = area.width.saturating_sub(2 + 2 * PROMPT_PAD_H);
    let prompt_h = app.prompt_state.height_hint(text_w, area.height);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "live-agent tab count; a handful of subprocesses, never near u16::MAX"
    )]
    let tab_h = if app.tabs.len() > 1 {
        app.tabs.len() as u16
    } else {
        0u16
    };
    // The queued-user rows sit above the matrix/tab row: prompts the human
    // submitted mid-turn, waiting for a tool or turn boundary. They read only
    // `UserSteering` from the typed inbox, then render through the same prompt
    // chrome path as committed prompt echoes.
    let queued = app.inbox.queued_user_messages();
    let queued_lines = if queued.is_empty() {
        Vec::new()
    } else {
        let w = area.width.saturating_sub(LEFT_MARGIN).min(READ_W);
        queued_prompt_rows(&queued, w, (area.height / 3).max(1) as usize)
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "queued rows capped at (area.height/3).max(1) when built — viewport-bounded"
    )]
    let queued_h = queued_lines.len() as u16;
    // The register's vertical budget is decided here, before the layout:
    // shown as the right-hand column when the focused session has pins and
    // the terminal is wide enough to spare the margin.  `content.width ==
    // area.width` (the vertical split keeps full width), so the threshold
    // reads off `area.width` directly.
    let focused = app.tabs.focused();
    let has_pins = app
        .tabs
        .viewport(focused)
        .is_some_and(|vp| !vp.pins().is_empty());
    let register_w = area.width.saturating_sub(LEFT_MARGIN + READ_W);
    let show_register = has_pins && register_w >= REGISTER_MIN_W;
    let layout = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1), // breathing row between output and chrome
        Constraint::Length(queued_h),
        Constraint::Length(tab_h),
        Constraint::Length(prompt_h),
        Constraint::Length(1), // rule_line: sits below prompt, above footer
        Constraint::Length(1),
    ])
    .split(area);
    let (content, queued_row, tab_row, prompt_row, status_row, footer_row) = (
        layout[0], layout[2], layout[3], layout[4], layout[5], layout[6],
    );
    // Split the content row by hand into the rail's left gutter, the
    // transcript, and — on a wide enough terminal — the register covering all
    // remaining columns to the right.  No scrollbar: the right side is the
    // register's, and scroll position reads as a magnitude in the rule line.
    // Capping the transcript at READ_W (rather than letting it expand) is what
    // keeps the register from ever narrowing prose: it claims only dead margin.
    let text_w = if show_register {
        READ_W
    } else {
        content.width.saturating_sub(LEFT_MARGIN)
    };
    let text_rect = Rect::new(content.x + LEFT_MARGIN, content.y, text_w, content.height);
    let register_rect = show_register.then(|| {
        Rect::new(
            content.x + LEFT_MARGIN + READ_W,
            content.y,
            register_w,
            content.height,
        )
    });
    // Inset the queued-user strip and rule line to share the transcript's
    // left gutter.
    let queued_rect = Rect::new(
        queued_row.x + LEFT_MARGIN,
        queued_row.y,
        queued_row.width.saturating_sub(LEFT_MARGIN),
        queued_row.height,
    );
    let status_rect = Rect::new(
        status_row.x + LEFT_MARGIN,
        status_row.y,
        status_row.width.saturating_sub(LEFT_MARGIN),
        status_row.height,
    );
    // Pre-render the register's content (the focused session's pins, in its
    // agent hue) before the borrow needed by `render_window`: the full
    // right column, shown only when the terminal is wide enough.
    let register_lines: Vec<Line<'static>> = match app.tabs.viewport(focused) {
        Some(vp) if show_register => {
            let hue = AGENT_HUES
                .get(vp.agent().0 as usize)
                .copied()
                .unwrap_or(AGENT_HUES[0]);
            line::render_register(vp.pins(), register_w, hue)
        }
        _ => Vec::new(),
    };
    let (mut lines, offset, scroll_pct) = match app.tabs.viewport_mut(focused) {
        Some(vp) => {
            let w = vp.render_window(text_rect.width, text_rect.height as usize);
            (w.lines, w.offset, w.scroll_pct)
        }
        None => (Vec::new(), 0, None),
    };
    paint_selection(app, &mut lines, offset);
    paint_hover(app, &mut lines, offset);
    app.gesture.record_frame(FrameGeom {
        text: text_rect,
        offset,
    });

    app.prompt_state.style_prompt(focused == app.tabs.root());
    // The rule_line reads the focused viewport's live phase: its label
    // (cloned to outlive the borrow) and its elapsed wall-time, which the
    // elapsed-wait bar encodes.
    let (phase, wait_elapsed) = app
        .tabs
        .viewport(focused)
        .map(|vp| (vp.phase_label().map(str::to_owned), vp.phase_elapsed()))
        .unwrap_or_default();
    let usage = app.total_usage;
    let last_input = app.last_input;
    let context_window = app.context_window;
    let status_model = app.status_model.clone();
    // The matrix replaces the tab bar when more than one session is
    // live: one row per agent, each owning its `Line` so the `'static`
    // draw closure captures no borrow of `app`.
    let matrix_lines = (app.tabs.len() > 1).then(|| {
        let rows = app.tabs.matrix_rows();
        matrix_bar(
            &rows,
            app.tabs.names(),
            focused,
            app.tabs.root(),
            app.tabs.dying_map(),
            app.matrix_sort,
        )
    });
    let prompt_hint = prompt_hint(
        app.tabs.root(),
        app.tabs.is_steerable(),
        app.tabs.names(),
        focused,
    );
    let overlay = app.overlay.as_ref();

    // Bracket the frame's terminal writes in a synchronized update so the
    // emulator buffers the whole diff and swaps it atomically.  Without
    // this, a tail-following redraw rewrites every visible cell each tick,
    // and a terminal scanning the screen mid-write shows a half-painted
    // frame — the tearing seen when a full page streams tool calls.
    // `End` is emitted on the error path too, so a failed draw never
    // strands the terminal in synchronized mode.
    execute!(io::stdout(), BeginSynchronizedUpdate)?;
    let drawn = term.draw(|f| {
        f.render_widget(Paragraph::new(lines.as_slice()), text_rect);
        // The register column — the focused session's pinned state,
        // shown only when wide enough, painted on the right edge.
        if let Some(reg) = register_rect {
            f.render_widget(Paragraph::new(register_lines), reg);
        }
        if !queued_lines.is_empty() {
            f.render_widget(Paragraph::new(queued_lines), queued_rect);
        }
        if let Some(matrix) = matrix_lines {
            f.render_widget(Paragraph::new(matrix), tab_row);
        }
        f.render_widget(
            Paragraph::new(rule_line(
                text_rect.width.min(READ_W) as usize,
                phase.as_deref(),
                wait_elapsed,
                scroll_pct,
                StatusReadout {
                    usage: &usage,
                    last_input,
                    context_window,
                    model: &status_model,
                },
            )),
            status_rect,
        );
        // The prompt region draws normally; the `/model` picker, when
        // open, floats over the whole frame below (its own [`Clear`]ed
        // centre), so it never displaces the input. Input lives in main
        // only; a subagent tab shows a watch-only hint in the prompt slot,
        // and the textarea keeps its draft for when the user tabs home.
        if let Some(line) = prompt_hint {
            let block = prompt_block(Style::default().fg(SLATE).add_modifier(Modifier::DIM));
            f.render_widget(Paragraph::new(line).block(block), prompt_row);
        } else {
            // The prompt's rounded border is exarch chrome, not the
            // editor's: the facade renders bare text, so the box is
            // drawn here and the editor fills its padded interior.
            let block = prompt_block(Style::default().fg(PINK));
            let inner = block.inner(prompt_row);
            f.render_widget(block, prompt_row);
            app.prompt_state.render(f, inner);
            // Show the terminal's native cursor at the edit point —
            // but not while a modal overlay owns the keyboard, or the
            // cursor would peek out beneath it.
            if overlay.is_none()
                && let Some(pos) = app.prompt_state.cursor_screen_position()
            {
                let x = inner.x + pos.0.min(inner.width.saturating_sub(1));
                let y = inner.y + pos.1.min(inner.height.saturating_sub(1));
                f.set_cursor_position(Position::new(x, y));
            }
        }
        f.render_widget(Paragraph::new(footer_hint()), footer_row);
        // Toast: short-lived copy confirmation, bottom-right.
        if let Some((n, ts)) = app.gesture.copy_toast()
            && ts.elapsed() < COPY_TOAST_TTL
        {
            let msg = format!("[{n} characters copied]");
            #[allow(
                clippy::cast_possible_truncation,
                reason = "fixed short toast string, then clamped to footer width"
            )]
            let w = (msg.len() as u16).min(footer_row.width);
            let r = Rect {
                x: footer_row.x + footer_row.width.saturating_sub(w),
                y: footer_row.y,
                width: w,
                height: 1,
            };
            f.render_widget(Paragraph::new(msg).style(Style::default().fg(LIME_HOT)), r);
        }
        // Last: the floating overlay (the `/model` picker or `/login`), over
        // the dimmed session.
        match overlay {
            Some(Overlay::Picker(p)) => p.render(f, area),
            Some(Overlay::Login(l)) => l.render(f, area),
            None => {}
        }
    });
    execute!(io::stdout(), EndSynchronizedUpdate)?;
    emit_tab_title(app);
    drawn?;
    Ok(())
}

/// Emit the terminal tab title: a spinner until the root has yielded to the
/// human input boundary, a block while the prompt is genuinely idle.
fn emit_tab_title(app: &App) {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into());
    let working = !app.inbox.waiting_for_input();
    let glyph = if working {
        SPINNER[(app.tabs.title_frame() / 4) as usize % SPINNER.len()]
    } else {
        '█'
    };
    let title = format!("{glyph} exarch: {cwd}");
    let seq = ral_core::ansi::osc_set_title(&title);
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Reverse-video the character range of the active selection that
/// falls within the visible window.
fn paint_selection(app: &App, lines: &mut [Line<'static>], offset: usize) {
    let Some((a, b)) = app.gesture.selection() else {
        return;
    };
    let (lo, hi) = (a.min(b), a.max(b));
    let (lo_row, lo_col) = lo;
    let (hi_row, hi_col) = hi;
    for (i, line) in lines.iter_mut().enumerate() {
        let row = offset + i;
        if row < lo_row || row > hi_row {
            continue;
        }
        if lo_row == hi_row {
            highlight_range(line, lo_col, hi_col);
        } else if row == lo_row {
            highlight_range(line, lo_col, u16::MAX);
        } else if row == hi_row {
            highlight_range(line, 0, hi_col);
        } else {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
}

/// Brighten the rail glyph of the hovered dialable block so the dial
/// target reads as a lit button under the pointer.  Only the block's
/// first visible row carries the rail glyph (body rows have none), so
/// the reverse lands on the leading span of that row alone; a block
/// whose header has scrolled off the top shows no mark until it
/// returns into view.
fn paint_hover(app: &App, lines: &mut [Line<'static>], offset: usize) {
    let Some(target) = app.gesture.hover() else {
        return;
    };
    let Some(vp) = app.tabs.viewport(app.tabs.focused()) else {
        return;
    };
    for (i, line) in lines.iter_mut().enumerate() {
        let row = offset + i;
        let head = row == 0 || vp.block_at(row - 1) != Some(target);
        if vp.block_at(row) == Some(target) && head {
            if let Some(glyph) = line.spans.first_mut() {
                glyph.style = glyph.style.add_modifier(Modifier::REVERSED);
            }
            break;
        }
    }
}

/// The watch-only banner shown in the prompt slot on a subagent tab,
/// or `None` on main where the textarea is editable.
fn prompt_hint(
    root: AgentId,
    focused_steerable: bool,
    names: &HashMap<AgentId, String>,
    focused: AgentId,
) -> Option<Line<'static>> {
    if focused == root || focused_steerable {
        return None;
    }
    let name = names.get(&focused).map_or("?", String::as_str);
    Some(Line::from(Span::styled(
        format!(" watching {name} — tab to main to steer "),
        Style::default()
            .fg(SLATE)
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
    )))
}

/// The prompt editor's rounded chrome in `border` ink — exarch's frame
/// around the (bare-text) editor, built once so the hint and editor paths
/// share one box.
fn prompt_block(border: Style) -> ratatui::widgets::Block<'static> {
    ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(border)
        .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H))
}

fn footer_hint() -> Line<'static> {
    let st = Style::default()
        .fg(SLATE)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let hint =
        " Tab pane | drag copy (⇧ native) | Ctrl-X Ctrl-E editor | Ctrl-C cancel | /quit to leave ";
    Line::from(Span::styled(hint, st))
}
