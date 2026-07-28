//! Frame drawing: [`draw`] paints every strip of the TUI into a [`Term`].

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
use super::matrix::matrix_bar;
use super::palette::{AGENT_HUES, LIME_HOT, PINK, READ_W, SLATE};
use super::select::highlight_range;
use super::status::rule_line;
use super::terminal::Term;

const PROMPT_PAD_H: u16 = 1;

/// Left gutter shared by the transcript, queued-prompt strip, and rule line,
/// so the rail sits off the terminal edge.
const LEFT_MARGIN: u16 = 2;
/// The register appears only with this many columns spare beside the
/// `READ_W`-capped transcript, and then takes all of them.
const REGISTER_MIN_W: u16 = 35;

/// Braille spinner for the terminal tab title; one glyph per four loop ticks.
const SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Where the content area sat in the last drawn frame.
#[derive(Clone, Copy)]
pub(super) struct FrameGeom {
    pub(super) text: Rect,
    /// First visible buffer row.
    pub(super) offset: usize,
}
impl FrameGeom {
    /// Map a mouse event to its scrolled buffer row and text-area column
    /// (0 = left edge), or `None` outside the content area.
    pub(super) fn buffer_coords(&self, me: MouseEvent) -> Option<(usize, u16)> {
        self.text
            .contains(Position::new(me.column, me.row))
            .then(|| {
                let row = self.offset + (me.row - self.text.y) as usize;
                (row, me.column - self.text.x)
            })
    }
}
/// Paint one frame into `term`.
pub(super) fn draw(app: &mut App, term: &mut Term) -> io::Result<()> {
    let (cols, rows) = size().unwrap_or((READ_W, 24));
    let area = Rect::new(0, 0, cols, rows);
    let text_w = area.width.saturating_sub(2 + 2 * PROMPT_PAD_H);
    let prompt_h = app.prompt_state.height_hint(text_w, area.height);
    // A lone root gets no tab row at all; a second tab of either kind brings
    // up the matrix, one row per tab, so a demoted agent is never invisible.
    let demoted = app.demoted();
    let promoted_rows = app.tabs.len() - demoted.len();
    let show_bar = !demoted.is_empty() || promoted_rows > 1;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "live-agent tab count; a handful of subprocesses, never near u16::MAX"
    )]
    let tab_h = if show_bar {
        app.tabs.len() as u16
    } else {
        0u16
    };
    // Prompts submitted mid-exchange and still queued: only `UserSteering`
    // surfaces, through the same chrome a committed prompt echo uses.
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
    // Settled before the layout: the vertical split keeps full width, so
    // `area.width` stands in for the content row's width.
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
        Constraint::Length(1), // rule_line
        Constraint::Length(1),
    ])
    .split(area);
    let (content, queued_row, tab_row, prompt_row, status_row, footer_row) = (
        layout[0], layout[2], layout[3], layout[4], layout[5], layout[6],
    );
    // Capping the transcript at READ_W is what keeps the register to dead
    // margin, so it never narrows prose.
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
    // Built before the `viewport_mut` borrow that `render_window` needs.
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
    // Owned copies from here down: the `'static` draw closure below may
    // capture no borrow of `app`.
    let (phase, wait_elapsed) = app
        .tabs
        .viewport(focused)
        .map(|vp| (vp.phase_label().map(str::to_owned), vp.phase_elapsed()))
        .unwrap_or_default();
    let usage = app.total_usage;
    let last_input = app.last_input;
    let context_window = app.context_window;
    let status_model = app.status_model.clone();
    let matrix_lines = show_bar.then(|| {
        let rows = app.tabs.matrix_rows();
        matrix_bar(
            &rows,
            app.tabs.names(),
            focused,
            app.tabs.root(),
            app.tabs.dying_map(),
            &demoted,
            app.matrix_sort,
        )
    });
    let prompt_hint = prompt_hint(
        app.tabs.root(),
        app.is_steerable(),
        app.tabs.names(),
        focused,
    );
    let overlay = app.overlay.as_ref();

    // A tail-following redraw rewrites every visible cell, so without an
    // atomic swap a terminal scanning mid-write shows half a frame.  `drawn?`
    // waits until after `End`: a failed draw must not strand the terminal in
    // synchronized mode.
    execute!(io::stdout(), BeginSynchronizedUpdate)?;
    let drawn = term.draw(|f| {
        f.render_widget(Paragraph::new(lines.as_slice()), text_rect);
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
                &usage,
                last_input,
                context_window,
                &status_model,
            )),
            status_rect,
        );
        if let Some(line) = prompt_hint {
            let block = prompt_block(Style::default().fg(SLATE).add_modifier(Modifier::DIM));
            f.render_widget(Paragraph::new(line).block(block), prompt_row);
        } else {
            let block = prompt_block(Style::default().fg(PINK));
            let inner = block.inner(prompt_row);
            f.render_widget(block, prompt_row);
            app.prompt_state.render(f, inner);
            // No native cursor while an overlay owns the keyboard, or it
            // peeks out from beneath it.
            if overlay.is_none()
                && let Some(pos) = app.prompt_state.cursor_screen_position()
            {
                let x = inner.x + pos.0.min(inner.width.saturating_sub(1));
                let y = inner.y + pos.1.min(inner.height.saturating_sub(1));
                f.set_cursor_position(Position::new(x, y));
            }
        }
        f.render_widget(Paragraph::new(footer_hint()), footer_row);
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
        // Last, so the overlay floats over every strip already painted.
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

/// Set the terminal tab title: a spinner until the root yields to the human,
/// a block once it waits.  Skipped when the composed title is unchanged.
fn emit_tab_title(app: &mut App) {
    let working = !app.inbox.waiting_for_input();
    let glyph = if working {
        SPINNER[(app.tabs.title_frame() / 4) as usize % SPINNER.len()]
    } else {
        '█'
    };
    let title = format!("{glyph} exarch: {}", app.cwd_basename);
    if app.last_title == title {
        return;
    }
    let seq = ral_core::ansi::osc_set_title(&title);
    let _ = std::io::stdout().write_all(seq.as_bytes());
    let _ = std::io::stdout().flush();
    app.last_title = title;
}

/// Reverse-video the part of the active selection inside the visible window.
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

/// Light the rail glyph of the hovered dialable block.  Only its first row
/// carries a glyph, so a block scrolled past its header shows no mark.
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

/// The watch-only banner for a subagent tab's prompt slot, or `None` where
/// the textarea is editable.
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

/// The prompt's rounded chrome in `border` ink: the editor renders bare text,
/// so the box is exarch's, and the hint and editor paths share it.
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
